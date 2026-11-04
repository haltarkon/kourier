//! "Show in Folder" after a save (F3b): the file manager selects the saved file
//! (`org.freedesktop.FileManager1.ShowItems`, through `kourier-desktop`); where that service is
//! not there the folder opens with the desktop's opener (`xdg-open`, the environment the person
//! gave Kourier, see [`crate::opener`]). The call runs on a thread of its own: the Qt thread
//! never waits for the bus.
//!
//! Where the save dialog starts ([`start_folder`]): the folder of the last save, else Downloads
//! (`user-dirs.dirs`, the freedesktop way), else the home folder. It looks at the file system
//! (`is_dir` of a folder the person chose long ago, which may be a network share that is gone),
//! so a front-end asks through [`start_folder_then`], which answers from a thread: the Qt
//! thread never waits for a mount.
//!
//! Log safety (§9.1): a path or a URI is never logged here; the log says only that the file
//! manager could not be reached.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long [`start_folder_then`] waits for the file system before it answers with the home
/// folder (a remembered folder on a share that does not answer must not keep the dialog away).
const FOLDER_WAIT: Duration = Duration::from_secs(2);

/// The `file://` URI of an absolute path, percent-encoded as the file manager expects.
pub fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The folder the save dialog starts in: `last` (the folder of the last save) while it is still
/// a folder, else the user's Downloads folder (`XDG_DOWNLOAD_DIR` of `user-dirs.dirs` in
/// `$XDG_CONFIG_HOME` or `~/.config`, else `~/Downloads`) while that is a folder, else the home
/// folder (else `/`).
pub fn start_folder(last: Option<&str>) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    if let Some(last) = last.map(PathBuf::from)
        && last.is_absolute()
        && last.is_dir()
    {
        return last;
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    let listed = config
        .and_then(|c| std::fs::read_to_string(c.join("user-dirs.dirs")).ok())
        .and_then(|text| download_dir_of(&text, home.as_deref()));
    let downloads = listed.or_else(|| home.as_ref().map(|h| h.join("Downloads")));
    if let Some(d) = downloads.filter(|d| d.is_dir()) {
        return d;
    }
    home.unwrap_or_else(|| PathBuf::from("/"))
}

/// [`start_folder`] without waiting for it: `done` gets the folder from a short thread (as
/// [`in_folder`] runs its call), at most [`FOLDER_WAIT`] later. A file system that does not answer
/// in that time (a dead network mount: `is_dir` can block in the kernel for minutes) gets the
/// home folder, looked at in no way, and the thread that looks is left to end by itself.
/// `done` runs on a thread of this module, never on the caller's.
pub fn start_folder_then(last: Option<String>, done: impl FnOnce(PathBuf) + Send + 'static) {
    folder_then(FOLDER_WAIT, move || start_folder(last.as_deref()), done);
}

/// [`start_folder_then`] with the look and the wait given (the tests slow the look down).
fn folder_then(
    wait: Duration,
    look: impl FnOnce() -> PathBuf + Send + 'static,
    done: impl FnOnce(PathBuf) + Send + 'static,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    // The waiter owns `done`: it answers once, with the look's folder, or the home folder when
    // the look is late or could not start (the sender is gone then).
    let waiter = std::thread::Builder::new()
        .name("start-folder".into())
        .spawn(move || done(rx.recv_timeout(wait).unwrap_or_else(|_| home_or_root())));
    if waiter.is_err() {
        tracing::warn!("the thread that waits for the save folder could not be started");
        return;
    }
    let looker = std::thread::Builder::new()
        .name("start-folder-look".into())
        .spawn(move || {
            // (A waiter that gave up is gone: the answer is not needed.)
            let _ = tx.send(look());
        });
    if looker.is_err() {
        tracing::warn!("the thread that looks for the save folder could not be started");
    }
}

/// The home folder from the environment (else `/`), with no look at the file system.
fn home_or_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `XDG_DOWNLOAD_DIR="$HOME/Downloads"` of a `user-dirs.dirs` file: the value with `$HOME` filled
/// in, when it is an absolute path.
fn download_dir_of(text: &str, home: Option<&Path>) -> Option<PathBuf> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("XDG_DOWNLOAD_DIR="))?;
    let value = line.split_once('=')?.1.trim().trim_matches('"');
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => PathBuf::from(format!("{}{rest}", home?.to_str()?)),
        None => PathBuf::from(value),
    };
    path.is_absolute().then_some(path)
}

/// Shows `file` in the user's file manager, selected; returns at once (the call is made on a
/// thread). `drop_env` is the front-end's list of the variables the opener takes out of its
/// child's environment ([`crate::opener::command`]).
pub fn in_folder(file: &Path, drop_env: Vec<String>) {
    let file = file.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("show-in-folder".into())
        .spawn(move || {
            if !selected_by_file_manager(&file) {
                open_folder_of(&file, &drop_env);
            }
        });
    if spawned.is_err() {
        tracing::warn!("the thread that shows a saved file could not be started");
    }
}

/// `ShowItems` on the session bus; whether the file manager took it.
fn selected_by_file_manager(file: &Path) -> bool {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    let uri = file_uri(file);
    runtime.block_on(async {
        let Ok(bus) = kourier_desktop::Bus::session().await else {
            return false;
        };
        kourier_desktop::file_manager::show_items(&bus, &uri)
            .await
            .is_ok()
    })
}

/// The fallback: the folder that holds `file`, opened by the desktop (nothing is selected).
fn open_folder_of(file: &Path, drop_env: &[String]) {
    let Some(folder) = file.parent() else {
        return;
    };
    let names: Vec<&str> = drop_env.iter().map(String::as_str).collect();
    let started =
        crate::opener::command(&file_uri(folder), &names).is_some_and(crate::opener::start);
    if !started {
        tracing::warn!("the folder of a saved file could not be opened");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_download_folder_is_read_from_the_user_dirs_file() {
        let home = Path::new("/home/ari");
        let file =
            "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Eingang\"\n";
        assert_eq!(
            download_dir_of(file, Some(home)),
            Some(PathBuf::from("/home/ari/Eingang"))
        );
        assert_eq!(
            download_dir_of("XDG_DOWNLOAD_DIR=\"/data/dl\"", None),
            Some(PathBuf::from("/data/dl"))
        );
        assert_eq!(download_dir_of("XDG_DOWNLOAD_DIR=\"$HOME/x\"", None), None);
        assert_eq!(download_dir_of("XDG_DOWNLOAD_DIR=\"rel/x\"", None), None);
        assert_eq!(download_dir_of("XDG_MUSIC_DIR=\"/m\"", None), None);
    }

    /// The folder `start_folder_then` hands over.
    fn folder_of(last: Option<String>) -> PathBuf {
        let (tx, rx) = std::sync::mpsc::channel();
        start_folder_then(last, move |folder| {
            let _ = tx.send(folder);
        });
        rx.recv_timeout(Duration::from_secs(30))
            .expect("the folder arrives through the callback")
    }

    #[test]
    fn the_folder_comes_through_the_callback_and_is_the_one_start_folder_finds() {
        let last = tempfile::tempdir().expect("a folder");
        let remembered = last.path().to_str().expect("UTF-8").to_owned();
        // A folder that is there: that one, as `start_folder` says.
        assert_eq!(folder_of(Some(remembered.clone())), last.path());
        assert_eq!(start_folder(Some(&remembered)), last.path());
        // One that is gone, a relative one and none: the same answer as the plain function's
        // (Downloads or the home folder of this process's environment).
        for last in [
            Some(last.path().join("gone").to_str().expect("UTF-8").to_owned()),
            Some("relative/folder".to_owned()),
            None,
        ] {
            assert_eq!(folder_of(last.clone()), start_folder(last.as_deref()));
        }
        // The callback does not run on the caller's thread.
        let caller = std::thread::current().id();
        let (tx, rx) = std::sync::mpsc::channel();
        start_folder_then(None, move |_| {
            let _ = tx.send(std::thread::current().id());
        });
        assert_ne!(
            rx.recv_timeout(Duration::from_secs(30)).expect("answer"),
            caller
        );
    }

    #[test]
    fn a_look_that_does_not_answer_gives_the_home_folder_after_the_wait() {
        // The look waits for the end of the test (a dead mount): the answer is the home folder,
        // from the waiter, and the late folder is dropped.
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let (tx, rx) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        folder_then(
            Duration::from_millis(50),
            move || {
                let _ = hold.recv_timeout(Duration::from_secs(30));
                PathBuf::from("/late")
            },
            move |folder| {
                let _ = tx.send(folder);
            },
        );
        let got = rx.recv_timeout(Duration::from_secs(30)).expect("an answer");
        assert_eq!(got, home_or_root());
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the look was waited for"
        );
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "one answer only"
        );
        let _ = release.send(());
    }

    #[test]
    fn a_file_uri_is_percent_encoded() {
        assert_eq!(
            file_uri(Path::new("/home/a b/c#d%e/ф.txt")),
            "file:///home/a%20b/c%23d%25e/%D1%84.txt"
        );
    }
}
