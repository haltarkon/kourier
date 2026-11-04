//! HIS API file (X9): the tool looks at its metadata and never opens it.

use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};

use super::error_of;
use super::preflight::{World, current_uid, write_api_file};
use crate::tg_run::preflight::check_api_file;

/// Runs the check on a thread and waits for it: a check that opened a pipe with no writer would hang
/// for good (the guard is a hang guard, not a time assertion).
fn guarded(keeper: &Path, dir_uid: u32, file_uid: u32) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    let keeper: PathBuf = keeper.to_owned();
    std::thread::spawn(move || {
        let _ = tx.send(error_of(check_api_file(&keeper, dir_uid, file_uid)));
    });
    rx.recv_timeout(std::time::Duration::from_secs(20))
        .expect("the check hung (did it open the API file?)")
}

/// X9: HIS API file. Missing: the recipe with names, never a path; a wrong mode, a link, a
/// directory, a pipe, another name, another owner, empty, too long, a loose keeper directory:
/// refused, and no check hangs or reads the file. Mutant: mode 0644 accepted.
#[test]
fn the_api_file_is_his_and_private() {
    let w = World::new("api");
    let api = w.api_file();
    let uid = current_uid();
    let run = || error_of(w.run(&w.args(&[])));
    // Missing: with the keeper root there or not, through a whole connect.
    std::fs::remove_file(&api).expect("remove");
    let error = run();
    assert!(error.contains("install -m 0600 /dev/null"), "{error}");
    assert!(
        error.contains("telegram-api") && error.contains("kourier-tg-run"),
        "{error}"
    );
    assert!(
        !error.contains(w.scratch.0.to_str().expect("utf-8")),
        "a path: {error}"
    );
    std::fs::remove_dir(w.keeper()).expect("remove keeper");
    assert!(run().contains("install -m 0600 /dev/null"));
    w.make_keeper_and_api_file();
    w.run(&w.args(&[])).expect("a private file passes");
    // The mode: anything but exactly 0600, a set-id bit included.
    for mode in [0o644, 0o640, 0o660, 0o666, 0o400, 0o700, 0o4600] {
        write_api_file(&api, "id=1\n", mode);
        let error = run();
        assert!(error.contains("0600"), "mode {mode:o}: {error}");
    }
    // The tool never reads it: any content passes (the app judges the form).
    write_api_file(&api, "not the two lines\n", 0o600);
    w.run(&w.args(&[])).expect("content is the app's business");
    // A link (to a good file), a directory, a pipe.
    let good = w.keeper().join("good");
    write_api_file(&good, "id=1\n", 0o600);
    std::fs::remove_file(&api).expect("remove");
    symlink(&good, &api).expect("link");
    assert!(run().contains("a link"));
    std::fs::remove_file(&api).expect("remove");
    std::fs::create_dir(&api).expect("dir");
    assert!(run().contains("a directory"));
    std::fs::remove_dir(&api).expect("remove");
    let c_path = std::ffi::CString::new(api.to_str().expect("utf-8")).expect("cstring");
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    assert!(guarded(&w.keeper(), uid, uid).contains("a pipe"));
    std::fs::remove_file(&api).expect("remove");
    // Another name (a hard link), empty, too long, exactly as long as allowed.
    write_api_file(&api, "id=1\n", 0o600);
    std::fs::hard_link(&api, w.keeper().join("second-name")).expect("hard link");
    assert!(run().contains("another name"));
    std::fs::remove_file(w.keeper().join("second-name")).expect("remove");
    write_api_file(&api, "", 0o600);
    assert!(run().contains("empty"));
    write_api_file(&api, &"x".repeat(257), 0o600);
    assert!(run().contains("longer than 256"));
    write_api_file(&api, &"x".repeat(256), 0o600);
    w.run(&w.args(&[])).expect("256 bytes are fine");
    // Another owner of the file (the directory is ours), another owner of the directory.
    let error = guarded(&w.keeper(), uid, uid.wrapping_add(1));
    assert!(error.contains("not owned by you"), "{error}");
    let error = guarded(&w.keeper(), uid.wrapping_add(1), uid);
    assert!(error.contains("not your private directory"), "{error}");
    // The directory's mode, and a directory that is a link or a file.
    for mode in [0o755, 0o750, 0o770, 0o500, 0o1700] {
        std::fs::set_permissions(w.keeper(), std::fs::Permissions::from_mode(mode)).expect("mode");
        let error = guarded(&w.keeper(), uid, uid);
        assert!(
            error.contains("not your private directory"),
            "{mode:o}: {error}"
        );
    }
    std::fs::set_permissions(w.keeper(), std::fs::Permissions::from_mode(0o700)).expect("mode");
    let link = w.scratch.0.join("keeper-link");
    symlink(w.keeper(), &link).expect("link");
    assert!(guarded(&link, uid, uid).contains("not your private directory"));
}

/// A rehearsal asks for none of it: no API file, no library.
#[test]
fn a_rehearsal_needs_no_api_file() {
    let mut w = World::new("api-rehearsal");
    w.env = vec![("container", "lxc".to_owned())];
    w.cgroup = "0::/.lxc\n".to_owned();
    std::fs::remove_file(w.api_file()).expect("remove");
    std::fs::remove_dir(w.keeper()).expect("remove keeper");
    w.run(&w.rehearsal_args())
        .expect("a rehearsal needs no API file");
    assert_eq!(w.library_calls.get(), 0, "a rehearsal checks no library");
}
