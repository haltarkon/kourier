//! `backup` (T10): the copy of a stopped profile. Every place is below a scratch directory of the
//! test's own; no app runs, no host is contacted; the list of test IDs in these tests is a synthetic
//! line, and no test shows a real value.

use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{World, error_of, mode_of};
use crate::threema_run::backup::{BackupEnv, backup_in};
use crate::threema_run::preflight::{app_lock_file, utc_compact};
use crate::threema_run::profile::{ProfileDir, State, ensure, open_lock_file, wipe_in};
use crate::threema_run::{ALLOW_FILE, BACKUPS_DIR};

/// 2025-11-04 09:53:20 UTC.
const NOW: u64 = 1_762_250_000_000;
const STAMP: &str = "20251104T095320Z";

fn profile(w: &World) -> ProfileDir {
    let dir = ProfileDir {
        dir: w.profile(),
        rehearsal: false,
    };
    ensure(&dir, &State::new(1)).expect("a profile");
    dir
}

/// Files of a profile that has run: the app's data (0600), the logs, the stamped copy (0100), the
/// plaintext playback copies, the supervisor's files, a link, and a file that has the list's name.
fn fill(dir: &ProfileDir) {
    let write = |rel: &str, text: &str, mode: u32| {
        let path = dir.dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("dirs");
        std::fs::write(&path, text).expect("a file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode");
    };
    write("kourier/data/core.db", "database one", 0o600);
    write("kourier/data/accounts/a1/backend.db", "database two", 0o600);
    write("kourier/config/kourier.toml", "networks-on = []\n", 0o600);
    write("threema-run/app.log", "a line\n", 0o600);
    write("threema-run/steps.jsonl", "{}\n", 0o600);
    write("threema-run/bin/kourier", "a program", 0o100);
    write("kourier/run/media/clip", "plaintext", 0o600);
    write("threema-run/supervisor.json", "{}", 0o600);
    write("threema-run/lock", "", 0o600);
    write(&format!("kourier/{ALLOW_FILE}"), "a name only", 0o600);
    symlink("/nonexistent-place", dir.dir.join("threema-run/dangling")).expect("a link");
}

type Vars = Box<dyn Fn(&str) -> Option<String>>;

struct Places {
    vars: Vars,
    root: PathBuf,
    target: PathBuf,
}

impl Places {
    fn new(w: &World) -> Self {
        let home = w.scratch.0.join("home");
        let data = w.data();
        Self {
            vars: Box::new(move |k| match k {
                "HOME" => Some(home.display().to_string()),
                "XDG_DATA_HOME" => Some(data.display().to_string()),
                _ => None,
            }),
            root: w.scratch.0.join("repo"),
            target: w.target(),
        }
    }

    fn run(
        &self,
        dir: &ProfileDir,
        to: Option<&Path>,
        allow_unencrypted: bool,
        encrypted: bool,
    ) -> anyhow::Result<Vec<String>> {
        let disk = move |_: &Path| encrypted.then(|| "dm-crypt".to_owned());
        backup_in(
            dir,
            to,
            allow_unencrypted,
            &BackupEnv {
                vars: &*self.vars,
                root: &self.root,
                target: &self.target,
                worktrees: &[],
                disk: &disk,
                now_ms: NOW,
                settle: Duration::ZERO,
            },
        )
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

#[test]
fn the_stamp_is_a_name_a_directory_may_have() {
    assert_eq!(utc_compact(0), "19700101T000000Z");
    assert_eq!(utc_compact(NOW), STAMP);
    assert_eq!(
        utc_compact(951_782_400_000 + 86_399_000),
        "20000229T235959Z"
    );
}

/// The copy of a stopped profile: where it goes, its modes, what it holds and what it leaves out; the
/// list of test IDs is never in it, wherever it stands; nothing printed names a path.
#[test]
fn a_stopped_profile_is_copied_to_the_keepers_backups_without_the_list_and_the_program() {
    let w = World::new("backup-whole");
    let dir = profile(&w);
    fill(&dir);
    // The keeper root's list (a synthetic line): not part of a profile, never copied, never touched.
    std::fs::write(w.allow_file(), "sha256:synthetic\n").expect("a list");
    let places = Places::new(&w);
    let lines = places.run(&dir, None, false, true).expect("a backup");
    let leaf = format!("p-{STAMP}");
    let copy = w.keeper().join(BACKUPS_DIR).join(&leaf);
    assert!(copy.is_dir(), "the copy is where the words say");
    assert_eq!(mode_of(&w.keeper().join(BACKUPS_DIR)), 0o700);
    assert_eq!(mode_of(&copy), 0o700);
    assert_eq!(mode_of(&copy.join("kourier/data")), 0o700);
    // What it holds, file for file, with its mode.
    for (rel, text) in [
        ("kourier/data/core.db", "database one"),
        ("kourier/data/accounts/a1/backend.db", "database two"),
        ("kourier/config/kourier.toml", "networks-on = []\n"),
        ("threema-run/app.log", "a line\n"),
    ] {
        assert_eq!(std::fs::read_to_string(copy.join(rel)).expect(rel), text);
        assert_eq!(mode_of(&copy.join(rel)), 0o600, "{rel}");
    }
    assert!(copy.join("threema-run/state.json").is_file());
    // What it leaves out.
    for rel in [
        "threema-run/bin",
        "kourier/run",
        "threema-run/supervisor.json",
        "threema-run/lock",
        "threema-run/dangling",
        &format!("kourier/{ALLOW_FILE}"),
    ] {
        assert!(!copy.join(rel).exists(), "{rel} is no part of a copy");
    }
    // The list: nowhere in the copy, and the original stands as it was.
    let mut stack = vec![copy.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).expect("a dir").flatten() {
            assert_ne!(e.file_name(), ALLOW_FILE, "no list in a copy");
            if e.path().is_dir() {
                stack.push(e.path());
            }
        }
    }
    assert_eq!(
        std::fs::read_to_string(w.allow_file()).expect("the list stands"),
        "sha256:synthetic\n"
    );
    // The words: the copy by name, the counts, what is left out, the sensitivity; no path.
    let text = lines.join("\n");
    assert!(
        text.contains(&leaf) && text.contains("backups directory"),
        "{text}"
    );
    assert!(
        text.contains("6 files") && text.contains("6 entries left out"),
        "{text}"
    );
    assert!(
        text.contains("The list of test IDs is never part of a copy."),
        "{text}"
    );
    assert!(
        text.contains("as sensitive as the profile itself"),
        "{text}"
    );
    assert!(
        !text.contains(&*w.scratch.0.to_string_lossy()),
        "a path in the words: {text}"
    );
    // The profile is as it was, and its lock is free again.
    assert!(dir.dir.join("threema-run/bin/kourier").exists());
    let lock = open_lock_file(&dir.lock_file()).expect("the lock file");
    lock.try_lock().expect("the profile's lock is free again");
    drop(lock);
    // A second copy in the same second is refused; a later one is another directory.
    let again = error_of(places.run(&dir, None, false, true));
    assert!(again.contains("exists already"), "{again}");
    // `wipe` leaves the backups alone (they are the point of one).
    let (_, status) = wipe_in(&dir, true, Duration::ZERO).expect("wipe");
    assert_eq!(status, 0);
    assert!(
        copy.join("kourier/data/core.db").is_file(),
        "the copy outlives its profile"
    );
}

/// `--to <dir>` makes `<profile>-<UTC>` inside an existing directory of the user's and touches
/// nothing else of it.
#[test]
fn to_names_the_directory_the_copy_is_made_in() {
    let w = World::new("backup-to");
    let dir = profile(&w);
    fill(&dir);
    let places = Places::new(&w);
    let elsewhere = w.scratch.0.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("a directory");
    std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o755)).expect("mode");
    let lines = places
        .run(&dir, Some(&elsewhere), false, true)
        .expect("a backup");
    assert_eq!(names(&elsewhere), [format!("p-{STAMP}")]);
    assert_eq!(
        mode_of(&elsewhere),
        0o755,
        "the user's directory keeps its mode"
    );
    assert_eq!(mode_of(&elsewhere.join(format!("p-{STAMP}"))), 0o700);
    assert!(lines.join("\n").contains("in the directory you named"));
    assert!(
        !w.keeper().join(BACKUPS_DIR).exists(),
        "no default place was made"
    );
}

/// While a supervisor holds the profile's lock, or the app holds its own, nothing is copied.
#[test]
fn nothing_is_copied_while_an_app_runs() {
    let w = World::new("backup-running");
    let dir = profile(&w);
    fill(&dir);
    let places = Places::new(&w);
    // The supervisor's lock.
    let held = open_lock_file(&dir.lock_file()).expect("the lock file");
    held.try_lock().expect("held");
    let error = error_of(places.run(&dir, None, false, true));
    assert!(
        error.contains("`threema-run stop` first") && error.contains("Nothing was copied"),
        "{error}"
    );
    drop(held);
    // The app's own lock, its supervisor gone.
    let app = app_lock_file(&dir);
    std::fs::create_dir_all(app.parent().expect("a parent")).expect("dirs");
    let own = open_lock_file(&app).expect("the app's lock file");
    own.try_lock().expect("held");
    let error = error_of(places.run(&dir, None, false, true));
    assert!(error.contains("still holds its own lock"), "{error}");
    drop(own);
    assert!(!w.keeper().join(BACKUPS_DIR).exists(), "nothing was made");
    places
        .run(&dir, None, false, true)
        .expect("the locks are free: a copy");
}

/// The refusals, each before anything is made: a directory that is no profile, a name no profile has,
/// places a login must not be copied to, a disk with no encryption, a place inside the profile.
#[test]
fn a_copy_of_a_login_is_refused_where_it_must_not_live() {
    let w = World::new("backup-refused");
    let dir = profile(&w);
    fill(&dir);
    let places = Places::new(&w);
    let nothing = |what: &str| {
        assert!(
            !w.keeper().join(BACKUPS_DIR).exists(),
            "{what}: something was made"
        );
    };
    // Not a profile of this tool's, and the reserved name.
    let foreign = ProfileDir {
        dir: w.keeper().join("foreign"),
        rehearsal: false,
    };
    std::fs::create_dir_all(foreign.dir.join("kourier")).expect("a directory");
    let error = error_of(places.run(&foreign, None, false, true));
    assert!(error.contains("no state file"), "{error}");
    let reserved = ProfileDir {
        dir: w.keeper().join(BACKUPS_DIR),
        rehearsal: false,
    };
    let error = error_of(places.run(&reserved, None, false, true));
    assert!(error.contains("not a profile name"), "{error}");
    nothing("a foreign directory");
    // `--to`: `..`, a missing directory, a place inside the profile, the keeper root itself.
    let dotted = w.scratch.0.join("a").join("..").join("b");
    let error = error_of(places.run(&dir, Some(&dotted), false, true));
    assert!(error.contains("`..`"), "{error}");
    let error = error_of(places.run(&dir, Some(&w.scratch.0.join("missing")), false, true));
    assert!(error.contains("existing directory"), "{error}");
    let inside = dir.dir.join("threema-run");
    let error = error_of(places.run(&dir, Some(&inside), false, true));
    assert!(error.contains("inside the profile"), "{error}");
    let error = error_of(places.run(&dir, Some(&w.keeper()), false, true));
    assert!(error.contains("directly in the keeper root"), "{error}");
    // The unsafe places: the projects tree, the build directory, the repository, the app's own data.
    for (what, place) in [
        ("the projects tree", w.scratch.0.join("home/projects/x")),
        ("the cargo target directory", w.target().join("x")),
        ("the repository", w.scratch.0.join("repo/x")),
        ("the app's own data directory", w.data().join("kourier/x")),
        (
            "the WhatsApp run tool's kept profiles",
            w.data().join("kourier-wa-run/x"),
        ),
    ] {
        std::fs::create_dir_all(&place).expect("a place");
        let error = error_of(places.run(&dir, Some(&place), false, true));
        assert!(
            error.contains("refusing to put a copy of a real login there"),
            "{what}: {error}"
        );
        assert!(names(&place).is_empty(), "{what}: something was copied");
    }
    // A disk with no encryption: refused, then copied with the user's word and a warning.
    let error = error_of(places.run(&dir, None, false, false));
    assert!(error.contains("no disk encryption"), "{error}");
    nothing("a plain disk");
    let lines = places
        .run(&dir, None, true, false)
        .expect("the user's word");
    assert!(
        lines[0].starts_with("warning: --allow-unencrypted-disk"),
        "{lines:?}"
    );
    assert!(
        w.keeper()
            .join(BACKUPS_DIR)
            .join(format!("p-{STAMP}"))
            .is_dir()
    );
}

/// A link on the way to the copy's place is refused (the copy would land where the link points).
#[test]
fn a_link_on_the_way_to_the_copy_is_refused() {
    let w = World::new("backup-link");
    let dir = profile(&w);
    fill(&dir);
    let places = Places::new(&w);
    let real = w.scratch.0.join("real");
    std::fs::create_dir(&real).expect("a directory");
    let link = w.scratch.0.join("link");
    symlink(&real, &link).expect("a link");
    let error = error_of(places.run(&dir, Some(&link), false, true));
    assert!(error.contains("not canonical"), "{error}");
    assert!(names(&real).is_empty());
}

/// A rehearsal's place is a throwaway: the places of a real login and the disk are not judged there.
#[test]
fn a_rehearsals_place_skips_the_places_and_the_disk() {
    let w = World::new("backup-rehearsal");
    let mut dir = profile(&w);
    dir.rehearsal = true;
    fill(&dir);
    let places = Places::new(&w);
    places
        .run(&dir, None, false, false)
        .expect("a rehearsal copies on a plain disk");
    assert!(
        w.keeper()
            .join(BACKUPS_DIR)
            .join(format!("p-{STAMP}"))
            .is_dir()
    );
}

/// A copy that fails half way is removed: it must not look like a whole one. (Permissions do not stop
/// the superuser, so the case is for an ordinary user.)
#[test]
fn a_copy_that_fails_half_way_is_removed() {
    // SAFETY: a plain read of the effective user id.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let w = World::new("backup-failed");
    let dir = profile(&w);
    fill(&dir);
    let secret = dir.dir.join("kourier/data/accounts/a1/backend.db");
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).expect("mode");
    let places = Places::new(&w);
    let error = error_of(places.run(&dir, None, false, true));
    assert!(error.contains("removed"), "{error}");
    assert!(
        names(&w.keeper().join(BACKUPS_DIR)).is_empty(),
        "no half copy stands"
    );
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).expect("mode");
}
