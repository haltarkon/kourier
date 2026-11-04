//! The Threema backend in the application (TH1, TH4 J6): the rule for when a start offers it, the
//! allow-list of test IDs a prepared run profile names, and the network over its parts with the host
//! name the application reads.
//!
//! Like WhatsApp's (`app.rs` `whatsapp_registration`), a RELEASE build never offers Threema. Unlike
//! WhatsApp, a debug build does not list it as "compiled in but off" unless it can link: the backend
//! can ([`kourier_threema::LINKING_BUILT`], TH4) AND the profile is a kept RUN PROFILE with a
//! non-empty list of test IDs ([`allowed_ids`], decision E1): an action that does nothing is not
//! shown, and Kourier links Threema only in a prepared test profile. A plain debug start therefore
//! keeps Threema absent; a hand-edited `networks-on` still registers it, and the backend then
//! refuses `StartLogin` at once (B1's gate, no code shown).
//!
//! THE LIST (decision C10, the form of `telegram.rs`' API file): [`allowed_ids`] reads the list ONLY
//! in a debug build inside a kept run profile ([`run_profile`]: `KOURIER_HOME` set, canonical, a
//! direct child of `<XDG_DATA_HOME or HOME/.local/share>/kourier-threema-run/`), from the FIXED file
//! `<keeper root>/threema-allowed-ids` (0600, owned by this user, its directory 0700 and owned by
//! this user; one line `sha256:<64 lower-case hex digits>` per ID, the digest of
//! `kourier-threema-allow-v1:` + the 8 ID characters: [`kourier_threema::AllowList`]). Anything else
//! is an EMPTY list (fail closed): no run profile, no file, a bad owner or mode, one malformed line
//! (the whole list goes, not that line). A file that is there but not used is told ONCE as a warning
//! whose words are fixed: never the path, a line, a digest or an ID. The digest keeps an ID out of
//! plain sight; it is not a secret. THE FILE IS THE ONLY GATE before the first socket to Threema in a
//! debug build: the backend refuses `StartLogin` before it makes a client while its list is empty
//! (B1), and with a list the click on Link dials Threema's production rendezvous relay at once, before
//! the QR code is shown and before any scan. The ID check at `EssentialData` is the next line (which
//! account is stored, not whether Threema is contacted), and a release build never reads the file or
//! lists Threema. Whoever writes the file lets a debug start of that profile contact Threema: only the
//! run tool (TR, not built yet) writes it, after the user's go in that session; agents and tests write
//! it only inside a temporary data directory.
//!
//! Nothing here opens a connection or starts a process.

use std::ffi::OsString;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use kourier_threema::AllowList;

use crate::app::{Build, host_name};
use crate::paths::HOME_ENV;

/// The directory of the kept run profiles of the Threema tests below the data directory.
pub(crate) const RUN_DIR: &str = "kourier-threema-run";
/// The list of test IDs in the keeper root.
pub(crate) const ALLOW_FILE: &str = "threema-allowed-ids";
/// A line is `sha256:` and 64 digits; a list is a handful of test IDs: anything larger is not one.
const ALLOW_FILE_MAX: u64 = 16 * 1024;
/// The prefix of a line.
const LINE_PREFIX: &str = "sha256:";

/// What a start does with the Threema backend of this build.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ThreemaRegistration {
    /// Registered: the network runs, accounts of it start.
    Registered,
    /// Compiled in and turned off: on K-A's "compiled in but off" list, not registered.
    Listed,
    /// Not offered at all: neither registered nor listed.
    Absent,
}

/// THE RULE for the Threema backend (lane decision Q2): a RELEASE build never registers and never
/// lists it, whatever the preferences say (no archive links a real Threema account until the lane's
/// real runs have landed and the user says so; `cargo xtask dist` also builds without the
/// `threema` feature and its check of the packed binary refuses Threema in either list). A debug
/// build registers it while `networks-on` names it (`on`); while it is off it is listed only when
/// this start can link (`linking`: [`linking`] of the run profile's list), else it is absent.
pub(crate) fn threema_registration(build: Build, on: bool, linking: bool) -> ThreemaRegistration {
    match (build, on, linking) {
        (Build::Release, _, _) => ThreemaRegistration::Absent,
        (Build::Debug, true, _) => ThreemaRegistration::Registered,
        (Build::Debug, false, true) => ThreemaRegistration::Listed,
        (Build::Debug, false, false) => ThreemaRegistration::Absent,
    }
}

/// Whether this start can link a Threema account: the backend can ([`kourier_threema::LINKING_BUILT`])
/// AND the run profile named at least one test ID (decision E1). Without a prepared profile the
/// list is empty, so a plain debug start does not list Threema.
pub(crate) fn linking(allowed: &AllowList) -> bool {
    kourier_threema::LINKING_BUILT && !allowed.is_empty()
}

/// The Threema backend over `parts`, with the host name of [`host_name`] for its device label
/// (`Kourier (<host>)`, [`kourier_threema::ThreemaNetwork::device_label`]): the backend reads no
/// host name of its own, and with the list of test IDs of [`allowed_ids`] (empty: every link is
/// refused at once).
pub(crate) fn threema_network(
    parts: kourier_threema::ThreemaParts,
    allowed: AllowList,
) -> kourier_threema::ThreemaNetwork {
    kourier_threema::ThreemaNetwork::new(
        parts
            .with_host_name(host_name().unwrap_or_default())
            .with_allowed(allowed),
    )
}

/// `${XDG_DATA_HOME:-$HOME/.local/share}/kourier-threema-run` (relative values ignored, as
/// `telegram.rs` and xtask's `keeper_root` do); `None` without either variable.
pub(crate) fn keeper_root(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let abs = |k: &str| {
        env(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let data = abs("XDG_DATA_HOME").or_else(|| abs("HOME").map(|h| h.join(".local/share")))?;
    Some(data.join(RUN_DIR))
}

/// The keeper root (canonical) when this process runs in a kept Threema run profile:
/// `KOURIER_HOME` is set, absolute, already canonical (no `..`, no symbolic link on the way) and
/// a DIRECT child of the canonical keeper root. `None` in every other case, also when either
/// directory does not exist.
pub(crate) fn run_profile(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let home = PathBuf::from(env(HOME_ENV).filter(|v| !v.is_empty())?);
    if !home.is_absolute() {
        return None;
    }
    let canonical = std::fs::canonicalize(&home).ok()?;
    if canonical != home {
        return None;
    }
    let root = std::fs::canonicalize(keeper_root(env)?).ok()?;
    (home.parent() == Some(root.as_path())).then_some(root)
}

/// Why the list of test IDs is not used (fixed words: never the file's content or path).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum AllowFileError {
    /// It is a symbolic link, a directory or another kind of file.
    #[error("the list of Threema test IDs is not a regular file")]
    NotAFile,
    /// Not owned by this user.
    #[error("the list of Threema test IDs is not owned by this user")]
    Owner,
    /// Readable or writable by others (not 0600).
    #[error("the list of Threema test IDs is not private (mode 0600)")]
    Mode,
    /// Its directory is not this user's private directory (0700).
    #[error(
        "the directory of the list of Threema test IDs is not private (mode 0700, this user's)"
    )]
    Parent,
    /// It could not be read.
    #[error("the list of Threema test IDs could not be read")]
    Read,
    /// Too large, not text, or a line that is not `sha256:` and 64 lower-case hex digits.
    #[error("the list of Threema test IDs is too large, not text, or has a line that is no digest")]
    Form,
}

/// The list in `<root>/threema-allowed-ids`: `Ok(None)` when there is no such file. The file is
/// opened without following a link (`O_NOFOLLOW`) and checked on its open descriptor.
pub(crate) fn read_allow_file(root: &Path) -> Result<Option<AllowList>, AllowFileError> {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    read_allow_file_as(root, uid, uid)
}

/// [`read_allow_file`] with the uids the directory and the file must belong to. Both are this
/// user's in the application; a test passes another one, because the owner checks cannot be
/// reached by a plain user otherwise.
fn read_allow_file_as(
    root: &Path,
    dir_uid: u32,
    file_uid: u32,
) -> Result<Option<AllowList>, AllowFileError> {
    let dir = std::fs::symlink_metadata(root).map_err(|_| AllowFileError::Parent)?;
    if !dir.is_dir() || dir.uid() != dir_uid || dir.mode() & 0o777 != 0o700 {
        return Err(AllowFileError::Parent);
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join(ALLOW_FILE));
    let mut file = match file {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // O_NOFOLLOW on a symbolic link: ELOOP.
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(AllowFileError::NotAFile),
        Err(_) => return Err(AllowFileError::Read),
    };
    let meta = file.metadata().map_err(|_| AllowFileError::Read)?;
    if !meta.file_type().is_file() {
        return Err(AllowFileError::NotAFile);
    }
    if meta.uid() != file_uid {
        return Err(AllowFileError::Owner);
    }
    if meta.mode() & 0o777 != 0o600 {
        return Err(AllowFileError::Mode);
    }
    if meta.len() > ALLOW_FILE_MAX {
        return Err(AllowFileError::Form);
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(ALLOW_FILE_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| AllowFileError::Read)?;
    if bytes.len() as u64 > ALLOW_FILE_MAX {
        return Err(AllowFileError::Form);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| AllowFileError::Form)?;
    parse_allow_file(text).map(Some)
}

/// The lines `sha256:<64 lower-case hex digits>` (blank lines and a trailing newline are fine,
/// nothing else is: one bad line is a bad list, never a shorter one).
fn parse_allow_file(text: &str) -> Result<AllowList, AllowFileError> {
    let mut digests = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let hex = line.strip_prefix(LINE_PREFIX).ok_or(AllowFileError::Form)?;
        digests.push(digest_of(hex).ok_or(AllowFileError::Form)?);
    }
    Ok(AllowList::from_digests(digests))
}

/// The 32 bytes of 64 lower-case hex digits; `None` for any other text.
fn digest_of(hex: &str) -> Option<[u8; 32]> {
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let (pairs, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    let mut digest = [0u8; 32];
    for (out, [high, low]) in digest.iter_mut().zip(pairs) {
        *out = (digit(*high)? << 4) | digit(*low)?;
    }
    Some(digest)
}

/// The IDs a link may bring: only when `debug` and only inside a run profile ([`run_profile`]),
/// from its keeper root's fixed file. EMPTY in every other case; a file that is there but unusable
/// is told once as a warning (its fixed words) and not used.
pub(crate) fn allowed_ids(debug: bool, env: &dyn Fn(&str) -> Option<OsString>) -> AllowList {
    if !debug {
        return AllowList::default();
    }
    let Some(root) = run_profile(env) else {
        return AllowList::default();
    };
    match read_allow_file(&root) {
        Ok(list) => list.unwrap_or_default(),
        Err(problem) => {
            tracing::warn!(%problem, "the list of Threema test IDs is not used");
            AllowList::default()
        }
    }
}

/// Test support shared with `app.rs`' tests of a start: the events a closure makes.
#[cfg(test)]
pub(crate) mod capture {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt as _;

    /// Every event `f` makes, as `(level, fields)`: a subscriber of the calling thread only.
    #[derive(Clone, Default)]
    struct Seen(Arc<Mutex<Vec<(tracing::Level, String)>>>);

    struct Capture(Seen);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Fields(String);
            impl tracing::field::Visit for Fields {
                fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                    use std::fmt::Write as _;
                    let _ = write!(self.0, "{}={:?} ", f.name(), v);
                }
            }
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            let mut seen = (self.0).0.lock().expect("lock");
            seen.push((*event.metadata().level(), fields.0));
        }
    }

    /// `f`'s result and every event it made on this thread, as `(level, fields)`.
    pub(crate) fn events_of<T>(f: impl FnOnce() -> T) -> (T, Vec<(tracing::Level, String)>) {
        let seen = Seen::default();
        let subscriber = tracing_subscriber::registry().with(Capture(seen.clone()));
        let out = tracing::subscriber::with_default(subscriber, f);
        let events = seen.0.lock().expect("lock").clone();
        (out, events)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::capture::events_of;
    use super::*;

    /// All eight cases, each with its expectation written out (never computed by the rule): a release
    /// build offers nothing; a debug build registers while on; while off it is listed only by a build
    /// that can link, and absent before.
    #[test]
    fn a_release_build_never_offers_threema_and_a_debug_build_lists_it_only_once_it_can_link() {
        use ThreemaRegistration::{Absent, Listed, Registered};
        let cases = [
            (Build::Release, false, false, Absent),
            (Build::Release, false, true, Absent),
            (Build::Release, true, false, Absent),
            (Build::Release, true, true, Absent),
            (Build::Debug, false, false, Absent),
            (Build::Debug, false, true, Listed),
            (Build::Debug, true, false, Registered),
            (Build::Debug, true, true, Registered),
        ];
        for (build, on, linking, expected) in cases {
            assert_eq!(
                threema_registration(build, on, linking),
                expected,
                "{build:?}, on={on}, linking={linking}"
            );
        }
    }

    /// The label the phone lists the device by is made from the host name the APPLICATION reads
    /// (`host_name`, which also names the device in the UI's texts), not read a second way.
    #[test]
    fn the_device_label_comes_from_the_apps_host_name() {
        let network = threema_network(
            kourier_threema::ThreemaParts::refusing(Default::default()),
            AllowList::default(),
        );
        let label = network.device_label();
        match crate::app::host_name() {
            // Whatever the name: a label made from a host says "Kourier (" (no early return for an
            // exotic name; how the backend cleans or cuts it is its own test).
            Some(host) if !host.trim().is_empty() => {
                assert!(
                    label.starts_with("Kourier ("),
                    "the application has a host name, but the device label has none"
                );
                if host.chars().all(|c| c.is_ascii_graphic()) && host.len() <= 48 {
                    assert!(
                        label == format!("Kourier ({host})"),
                        "the device label is not \"Kourier\" and the application's host name"
                    );
                }
            }
            _ => assert!(
                label == "Kourier",
                "without a host name the device label is plain Kourier"
            ),
        }
    }

    // ----- the list of test IDs (TH4 J6, decisions C10 and E1) -----

    /// `printf 'kourier-threema-allow-v1:TESTID01' | sha256sum` (the vector of `kourier_threema`'s
    /// own digest test), the line of the file, and a second synthetic ID's.
    const LINE_01: &str = "sha256:871ad9802d67bec50e0d5a9d2c4113efec962437723daac9d86b8f1e0ab82a87";
    /// `printf 'kourier-threema-allow-v1:TESTID02' | sha256sum`.
    const LINE_02: &str = "sha256:020a216fe38ffd37ed19361ee27306c82e2db07b3bf215f83353a2daccc05afe";

    /// A scratch data directory with the keeper root `data/kourier-threema-run` (0700) and the
    /// profile `p` below it; the environment names the data directory and `KOURIER_HOME`.
    struct Scratch {
        _tmp: tempfile::TempDir,
        data: PathBuf,
        root: PathBuf,
        profile: PathBuf,
    }

    fn scratch() -> Scratch {
        let tmp = tempfile::tempdir().expect("tmp");
        // The temp dir itself may sit below a symbolic link (/tmp): work on its canonical path.
        let base = std::fs::canonicalize(tmp.path()).expect("canonical");
        let data = base.join("data");
        let root = data.join(RUN_DIR);
        let profile = root.join("p");
        std::fs::create_dir_all(&profile).expect("dirs");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        Scratch {
            _tmp: tmp,
            data,
            root,
            profile,
        }
    }

    fn env_of(pairs: Vec<(&'static str, OsString)>) -> impl Fn(&str) -> Option<OsString> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
    }

    fn in_profile(s: &Scratch) -> impl Fn(&str) -> Option<OsString> {
        env_of(vec![
            ("XDG_DATA_HOME", s.data.clone().into_os_string()),
            (HOME_ENV, s.profile.clone().into_os_string()),
        ])
    }

    fn write_list(s: &Scratch, bytes: &[u8], mode: u32) {
        let path = s.root.join(ALLOW_FILE);
        // A file left at 0400 by the loop before cannot be written again: write it anew.
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, bytes).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    /// The list a start would use for `env` (debug build), and every event it logged.
    fn allowed_in(
        debug: bool,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> (AllowList, Vec<(tracing::Level, String)>) {
        events_of(|| allowed_ids(debug, env))
    }

    /// C10: the keeper root follows XDG_DATA_HOME, else HOME, and ignores relative values.
    #[test]
    fn the_keeper_root_follows_xdg_data_home() {
        let xdg = env_of(vec![("XDG_DATA_HOME", "/d".into()), ("HOME", "/h".into())]);
        assert_eq!(
            keeper_root(&xdg),
            Some(PathBuf::from("/d/kourier-threema-run"))
        );
        let home = env_of(vec![("XDG_DATA_HOME", "rel".into()), ("HOME", "/h".into())]);
        assert_eq!(
            keeper_root(&home),
            Some(PathBuf::from("/h/.local/share/kourier-threema-run"))
        );
        assert_eq!(keeper_root(&env_of(Vec::new())), None);
    }

    /// C10: only a canonical DIRECT child of the keeper root is a run profile (a Telegram run
    /// profile is none: its keeper root is another directory).
    #[test]
    fn the_run_profile_is_a_canonical_direct_child_of_the_keeper_root() {
        let s = scratch();
        assert_eq!(run_profile(&in_profile(&s)), Some(s.root.clone()));
        let at = |home: PathBuf| {
            run_profile(&env_of(vec![
                ("XDG_DATA_HOME", s.data.clone().into_os_string()),
                (HOME_ENV, home.into_os_string()),
            ]))
        };
        assert_eq!(at(s.root.clone()), None, "the keeper root itself");
        let deeper = s.profile.join("q");
        std::fs::create_dir_all(&deeper).expect("dir");
        assert_eq!(at(deeper), None, "a grandchild");
        assert_eq!(at(s.profile.join("..").join("p")), None, "a path with ..");
        let telegram = s.data.join("kourier-tg-run").join("p");
        std::fs::create_dir_all(&telegram).expect("dir");
        assert_eq!(at(telegram), None, "a Telegram run profile");
        let elsewhere = s.data.join("other");
        std::fs::create_dir_all(&elsewhere).expect("dir");
        assert_eq!(at(elsewhere.clone()), None, "outside the keeper root");
        let link = s.root.join("link");
        std::os::unix::fs::symlink(&elsewhere, &link).expect("symlink");
        assert_eq!(at(link), None, "a symbolic link into the keeper root");
        assert_eq!(
            at(s.root.join("missing")),
            None,
            "a profile that does not exist"
        );
        assert_eq!(
            run_profile(&env_of(vec![(
                "XDG_DATA_HOME",
                s.data.clone().into_os_string()
            )])),
            None,
            "no KOURIER_HOME"
        );
    }

    /// C10 and E1: the list is read only in a debug build inside the run profile, from a private
    /// regular file of this user in a private directory; a missing file is an empty list and says
    /// nothing. Mutants: the `debug` check removed -> the release case gets the list; the run
    /// profile check removed -> the outside case gets it; the mode check removed -> the 0644 case.
    #[test]
    fn the_list_is_read_only_in_the_run_profile_of_a_debug_build_and_only_0600() {
        let s = scratch();
        let env = in_profile(&s);
        let (none, events) = allowed_in(true, &env);
        assert!(none.is_empty(), "a list without a file");
        assert!(
            events.is_empty(),
            "a missing file is no warning: {events:?}"
        );
        assert_eq!(read_allow_file(&s.root), Ok(None));

        write_list(&s, format!("{LINE_01}\n").as_bytes(), 0o600);
        let (list, events) = allowed_in(true, &env);
        assert!(list.contains("TESTID01") && list.len() == 1, "{list:?}");
        assert!(!list.contains("TESTID02") && !list.contains("testid01"));
        assert!(events.is_empty(), "a good list says nothing: {events:?}");
        let (release, events) = allowed_in(false, &env);
        assert!(release.is_empty(), "a release build read the list");
        assert!(events.is_empty(), "a release build looked at the file");
        let outside = env_of(vec![
            ("XDG_DATA_HOME", s.data.clone().into_os_string()),
            (HOME_ENV, s.root.clone().into_os_string()),
        ]);
        let (elsewhere, events) = allowed_in(true, &outside);
        assert!(elsewhere.is_empty(), "read outside the run profile");
        assert!(
            events.is_empty(),
            "outside a run profile nothing is looked at"
        );
        let (unset, _) = allowed_in(true, &env_of(Vec::new()));
        assert!(unset.is_empty(), "no environment, no list");

        for mode in [0o644, 0o640, 0o604, 0o700, 0o400, 0o660] {
            write_list(&s, format!("{LINE_01}\n").as_bytes(), mode);
            assert_eq!(
                read_allow_file(&s.root),
                Err(AllowFileError::Mode),
                "{mode:o}"
            );
            let (list, events) = allowed_in(true, &env);
            assert!(list.is_empty(), "{mode:o}: a list in a file others read");
            assert_eq!(events.len(), 1, "{mode:o}: {events:?}");
        }
        write_list(&s, format!("{LINE_01}\n").as_bytes(), 0o600);
        std::fs::set_permissions(&s.root, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(read_allow_file(&s.root), Err(AllowFileError::Parent));
        let (list, events) = allowed_in(true, &env);
        assert!(list.is_empty(), "a list in a directory others read");
        assert_eq!(events.len(), 1, "{events:?}");
        std::fs::set_permissions(&s.root, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        assert_eq!(
            read_allow_file(&s.root).map(|l| l.map(|l| l.len())),
            Ok(Some(1))
        );

        // A link, even to a good private file, is not the file.
        let real = s.data.join("elsewhere");
        std::fs::write(&real, format!("{LINE_01}\n")).expect("write");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        std::fs::remove_file(s.root.join(ALLOW_FILE)).expect("rm");
        std::os::unix::fs::symlink(&real, s.root.join(ALLOW_FILE)).expect("symlink");
        assert_eq!(read_allow_file(&s.root), Err(AllowFileError::NotAFile));
        let (list, events) = allowed_in(true, &env);
        assert!(list.is_empty() && events.len() == 1, "{events:?}");
    }

    /// A FIFO at `path` (mode 0600), made with `mkfifo`.
    fn make_fifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt as _;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("no NUL in a path");
        // SAFETY: `name` is a valid NUL-terminated path that outlives the call.
        let rc = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
        // (the umask may have taken bits off)
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }

    /// G3: what a start does with something that is not a file at the fixed name (`make` puts it
    /// there): `NotAFile`, an EMPTY list and ONE warning, and the start does not hang. The read runs
    /// on a thread the test waits for with a bound: a FIFO opened without `O_NONBLOCK` waits for a
    /// writer that never comes.
    fn no_list_at_the_fixed_name(kind: &str, make: impl FnOnce(&Path)) {
        let s = scratch();
        let path = s.root.join(ALLOW_FILE);
        make(&path);
        let (data, profile, root) = (s.data.clone(), s.profile.clone(), s.root.clone());
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let env = env_of(vec![
                ("XDG_DATA_HOME", data.into_os_string()),
                (HOME_ENV, profile.into_os_string()),
            ]);
            let read = read_allow_file(&root);
            let (list, events) = events_of(|| allowed_ids(true, &env));
            let _ = sender.send((read, list.is_empty(), events));
        });
        let answer = receiver.recv_timeout(std::time::Duration::from_secs(10));
        if matches!(answer, Err(std::sync::mpsc::RecvTimeoutError::Timeout)) {
            // Let a reader stuck in `open` go (a writer arrives), so no thread is left behind.
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path);
        }
        let (read, empty, events) = answer.unwrap_or_else(|e| {
            panic!("{kind}: the read of the list did not come back ({e}): the start would hang")
        });
        assert_eq!(read, Err(AllowFileError::NotAFile), "{kind}");
        assert!(empty, "{kind}: a list came out of it");
        assert_eq!(events.len(), 1, "{kind}: one warning is due: {events:?}");
        assert_eq!(events[0].0, tracing::Level::WARN, "{kind}");
        assert!(
            events[0].1.contains(&AllowFileError::NotAFile.to_string()),
            "{kind}: {events:?}"
        );
    }

    /// G3: a directory at the fixed name. Mutant: the regular-file check removed -> the directory
    /// (made with the default mode) is `Mode`, not `NotAFile`.
    #[test]
    fn a_directory_at_the_fixed_name_is_no_list() {
        no_list_at_the_fixed_name("directory", |path| {
            std::fs::create_dir(path).expect("dir");
        });
    }

    /// G3: a FIFO (mode 0600, no content) at the fixed name. Mutants: the regular-file check removed
    /// -> it is an empty list with no warning at all; `O_NONBLOCK` removed -> the `open` never
    /// returns (the test's bound ends it after 10 s).
    #[test]
    fn a_fifo_at_the_fixed_name_is_no_list_and_does_not_hang_the_start() {
        no_list_at_the_fixed_name("fifo", make_fifo);
    }

    /// C10: the owner of the file and of its directory are checked. A plain user has no second
    /// uid, so the checks take the uids as arguments and the test passes the wrong one for each.
    /// Mutants: the file's owner check removed -> `Owner` is not returned; the directory's
    /// removed -> `Parent` is not returned.
    #[test]
    fn the_owner_of_the_list_and_of_its_directory_is_checked() {
        let s = scratch();
        write_list(&s, format!("{LINE_01}\n").as_bytes(), 0o600);
        let mine = std::fs::metadata(&s.root).expect("meta").uid();
        assert_eq!(
            read_allow_file_as(&s.root, mine, mine).map(|l| l.map(|l| l.len())),
            Ok(Some(1))
        );
        assert_eq!(
            read_allow_file_as(&s.root, mine, mine + 1).map(|l| l.is_some()),
            Err(AllowFileError::Owner),
            "a file of another owner was read"
        );
        assert_eq!(
            read_allow_file_as(&s.root, mine + 1, mine).map(|l| l.is_some()),
            Err(AllowFileError::Parent),
            "a directory of another owner was trusted"
        );
    }

    /// The forms a good list may have: more than one ID, blank lines, a missing last newline,
    /// Windows line ends, the same line twice, an empty file (an empty list).
    #[test]
    fn a_list_may_hold_several_ids_and_blank_lines() {
        let s = scratch();
        let env = in_profile(&s);
        for (text, count) in [
            (format!("{LINE_01}\n{LINE_02}\n"), 2),
            (format!("\n{LINE_01}\n\n{LINE_02}"), 2),
            (format!("{LINE_01}\r\n{LINE_02}\r\n"), 2),
            (format!("{LINE_01}\n{LINE_01}\n"), 1),
            (format!("  {LINE_02}  \n"), 1),
            (String::new(), 0),
            ("\n\n".to_owned(), 0),
        ] {
            write_list(&s, text.as_bytes(), 0o600);
            let (list, events) = allowed_in(true, &env);
            assert_eq!(list.len(), count, "{text:?}");
            assert!(events.is_empty(), "{text:?}: {events:?}");
        }
        write_list(&s, format!("{LINE_01}\n{LINE_02}\n").as_bytes(), 0o600);
        let (list, _) = allowed_in(true, &env);
        assert!(list.contains("TESTID01") && list.contains("TESTID02"));
        assert!(!list.contains("TESTID03"));
    }

    /// C10: one malformed line empties the WHOLE list (fail closed: never the lines before it) and
    /// is told by ONE warning that names no path, line, digest or ID. Mutant: a bad line skipped
    /// instead of failing the list -> the good line before it is kept.
    #[test]
    fn a_malformed_line_empties_the_list_and_warns_once_without_content() {
        let s = scratch();
        let env = in_profile(&s);
        let upper = LINE_01.replace("871ad980", "871AD980");
        let short = &LINE_01[..LINE_01.len() - 1];
        let long = format!("{LINE_01}0");
        let bads: Vec<Vec<u8>> = vec![
            format!("{LINE_01}\nTESTID01\n").into_bytes(),
            format!("{LINE_01}\n{upper}\n").into_bytes(),
            format!("{LINE_01}\n{short}\n").into_bytes(),
            format!("{LINE_01}\n{long}\n").into_bytes(),
            format!("{LINE_01}\n{}\n", LINE_01.replace("sha256:", "sha256=")).into_bytes(),
            format!("{LINE_01}\n{}\n", LINE_01.replace("sha256:", "SHA256:")).into_bytes(),
            format!("{LINE_01}\n{}\n", LINE_01.replace("sha256:", "")).into_bytes(),
            format!("{LINE_01}\n{}g\n", &LINE_01[..LINE_01.len() - 1]).into_bytes(),
            format!("{LINE_01} {LINE_02}\n").into_bytes(),
            format!("{LINE_01}\n# a comment\n").into_bytes(),
            format!("{LINE_01}\n").repeat(1000).into_bytes(),
            [LINE_01.as_bytes(), b"\n", &[0xff, 0xfe, b'\n']].concat(),
        ];
        for bad in bads {
            write_list(&s, &bad, 0o600);
            let (list, events) = allowed_in(true, &env);
            assert!(list.is_empty(), "a bad list was used in part");
            assert_eq!(events.len(), 1, "one event for a bad list: {events:?}");
            let (level, fields) = &events[0];
            assert_eq!(*level, tracing::Level::WARN);
            for secret in [
                s.root.to_str().expect("path"),
                s.data.to_str().expect("path"),
                "TESTID",
                "871ad980",
                "871AD980",
                "020a216f",
                "sha256",
                "elsewhere",
            ] {
                assert!(
                    !fields.contains(secret),
                    "the warning names {secret:?}: {fields}"
                );
            }
        }
        // The warning's words are the problem's fixed words.
        write_list(&s, b"nonsense\n", 0o600);
        let (_, events) = allowed_in(true, &env);
        assert!(
            events[0].1.contains(&AllowFileError::Form.to_string()),
            "{events:?}"
        );
    }

    /// The problems' words never hold a path, a line or a digest.
    #[test]
    fn the_problems_are_fixed_words() {
        for problem in [
            AllowFileError::NotAFile,
            AllowFileError::Owner,
            AllowFileError::Mode,
            AllowFileError::Parent,
            AllowFileError::Read,
            AllowFileError::Form,
        ] {
            let words = problem.to_string();
            assert!(
                words.starts_with("the list of Threema test IDs")
                    || words.starts_with("the directory of the list of Threema test IDs"),
                "{words}"
            );
            assert!(
                !words.contains('/') && !words.contains("871ad980"),
                "{words}"
            );
        }
    }

    /// E1: this start can link only when the backend can and the run profile's list is not empty.
    /// Literal expectations (the backend can link since TH4); mutants: `LINKING_BUILT = false` ->
    /// the second line fails; `&&` -> `||` -> the third.
    #[test]
    fn a_start_can_link_only_with_a_non_empty_list() {
        assert!(linking(&AllowList::from_ids(&["TESTID01"])));
        assert!(!linking(&AllowList::default()), "an empty list can link");
    }
}
