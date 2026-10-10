//! Process hardening (architecture §9.4).
//!
//! Release builds make the process non-dumpable (`PR_SET_DUMPABLE 0`): no core dump can hold
//! keys or messages, and other processes of the same user can neither attach with `ptrace`
//! nor read `/proc/<pid>/mem`, `maps` or `smaps`. Debug builds keep dumps and debuggers.
//!
//! The cargo feature `dumpable` leaves a release build dumpable. Only `cargo xtask perf` uses
//! it: memory is measured from `/proc/<pid>/smaps_rollup`, which a non-dumpable process hides
//! from its own user (§1.3). Such a build is for measuring, not for daily use.
//!
//! `KOURIER_DEV_NO_DUMP=1` makes a debug build non-dumpable like a release build. A debug
//! build that holds a real account's keys (`cargo xtask smoke`) sets it: a crash, a failed
//! debug assertion or an abort would otherwise leave those keys in a core dump (this machine
//! pipes every dump to `systemd-coredump`, which keeps it under `/var/lib/systemd/coredump`).
//! The kernel writes no dump at all for a process whose dumpable flag is 0.

/// What a release build with the cargo feature `dumpable` says on stderr when it starts.
/// `cargo xtask install` refuses a binary that holds this text.
pub const MEASURING_BUILD_NOTE: &str = "kourier: measuring build (cargo feature dumpable): its memory can be read by other processes of this user; not for real accounts";

/// Whether this build makes itself non-dumpable at start: release builds without the cargo
/// feature `dumpable`.
pub const fn denies_dumps() -> bool {
    !cfg!(debug_assertions) && !cfg!(feature = "dumpable")
}

/// The environment variable that makes a debug build non-dumpable (`1`).
pub const NO_DUMP_ENV: &str = "KOURIER_DEV_NO_DUMP";

/// Whether `env` asks a debug build to make itself non-dumpable ([`NO_DUMP_ENV`] is `1`).
/// Release builds decide by themselves ([`denies_dumps`]) and never read it.
pub fn asked_to_deny_dumps(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    cfg!(debug_assertions) && env(NO_DUMP_ENV).is_some_and(|v| v == "1")
}

/// Makes the process non-dumpable when this build does that ([`denies_dumps`]) or a debug
/// build is asked to ([`NO_DUMP_ENV`]). `main` calls it before any key is loaded. Returns
/// whether the process is non-dumpable afterwards.
pub fn deny_dumps() -> bool {
    deny_dumps_if(denies_dumps() || asked_to_deny_dumps(|k| std::env::var_os(k)))
}

fn deny_dumps_if(deny: bool) -> bool {
    if deny {
        // SAFETY: PR_SET_DUMPABLE only changes a flag of this process; the unused arguments
        // must be zero.
        let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
        if rc != 0 {
            // No key is in memory yet, and nothing here is secret: say so and go on.
            eprintln!("kourier: cannot make the process non-dumpable");
        }
    }
    if cfg!(feature = "dumpable") && !cfg!(debug_assertions) {
        eprintln!("{MEASURING_BUILD_NOTE}");
    }
    !is_dumpable()
}

/// Whether the kernel would write a core dump of this process (and lets its user read
/// `/proc/<pid>/smaps_rollup`).
pub fn is_dumpable() -> bool {
    // SAFETY: PR_GET_DUMPABLE reads a flag of this process; the unused arguments must be zero.
    let rc = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
    rc != 0
}

/// Sets a panic hook that makes ONE line of every panic, tells it to the log and to `sink`,
/// and then runs the hook that was set before (the default one prints the panic's message to
/// stderr, as ever).
///
/// The line names the place and the thread only: `panicked at <file>:<line>:<column> on thread
/// "<name>"`. The panic's message is left out on purpose: `unwrap` and `expect` print the value
/// they failed on and an `assert_eq!` both sides, which can be a name, a number or a message
/// text (architecture §9.1). The line passes [`crate::logging::redact`] first, so `sink` gets
/// what the log got. The front-end gives its desktop's crash reporter as the sink (KDE: the
/// message `KCrash` shows); a front-end without one passes a function that does nothing.
///
/// Call it once, early in `main`; a second call chains a second hook that sends the line again.
/// `sink` runs inside the panic: it must be quick and must not panic (that aborts).
pub fn install_panic_hook(sink: fn(&str)) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|at| (at.file(), at.line(), at.column()));
        let thread = std::thread::current();
        let line = panic_line(location, thread.name());
        let line = crate::logging::redact(&line);
        tracing::error!("{line}");
        sink(&line);
        previous(info);
    }));
}

/// The line [`install_panic_hook`] makes: a place and a thread, no message.
fn panic_line(location: Option<(&str, u32, u32)>, thread: Option<&str>) -> String {
    let place = match location {
        Some((file, line, column)) => format!("{}:{line}:{column}", shown_file(file)),
        None => "an unknown place".to_owned(),
    };
    format!(
        "panicked at {place} on thread {:?}",
        thread.unwrap_or("<unnamed>")
    )
}

/// A source path without what names the machine or its user. Cargo gives a workspace crate's
/// files relative to the workspace (`crates/…`) and the standard library's as `/rustc/<hash>/…`:
/// both stay. A dependency's file is `$CARGO_HOME/registry/src/<index>/<crate>-<version>/…`:
/// the part from the crate on stays. Any other absolute path (generated code below `target/`)
/// keeps its file name only.
fn shown_file(file: &str) -> &str {
    if let Some((_, rest)) = file.split_once("/registry/src/") {
        return rest.split_once('/').map_or(rest, |(_index, krate)| krate);
    }
    if file.starts_with('/') && !file.starts_with("/rustc/") {
        return file.rsplit('/').next().unwrap_or(file);
    }
    file
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_builds_and_measuring_builds_stay_dumpable() {
        // Tests are debug builds (or release builds of a test profile with the feature off):
        // the constant must agree with the build, and the call must do what it says.
        assert_eq!(
            denies_dumps(),
            !cfg!(debug_assertions) && !cfg!(feature = "dumpable")
        );
        if !denies_dumps() {
            assert!(is_dumpable(), "a test process starts dumpable");
            assert!(
                !deny_dumps_if(false),
                "nothing changes in a debug or dumpable build"
            );
            assert!(is_dumpable());
        }
    }

    /// Only `1` asks, and only a debug build listens.
    #[test]
    fn a_debug_build_is_asked_with_the_variable() {
        let with = |value: Option<&'static str>| {
            asked_to_deny_dumps(move |k| {
                (k == NO_DUMP_ENV)
                    .then_some(value)
                    .flatten()
                    .map(std::ffi::OsString::from)
            })
        };
        assert_eq!(with(Some("1")), cfg!(debug_assertions));
        for other in [None, Some(""), Some("0"), Some("true"), Some("yes")] {
            assert!(!with(other), "{other:?}");
        }
    }

    #[test]
    fn the_panic_line_is_a_place_and_a_thread() {
        assert_eq!(
            panic_line(
                Some(("crates/kourier-app/src/x.rs", 12, 5)),
                Some("kourier-rt")
            ),
            "panicked at crates/kourier-app/src/x.rs:12:5 on thread \"kourier-rt\""
        );
        assert_eq!(
            panic_line(None, None),
            "panicked at an unknown place on thread \"<unnamed>\""
        );
        // A thread name cannot make a second line.
        assert!(!panic_line(None, Some("a\nb")).contains('\n'));
    }

    #[test]
    fn a_panic_line_names_no_machine_or_user() {
        for (file, shown) in [
            ("crates/kourier-app/src/x.rs", "crates/kourier-app/src/x.rs"),
            (
                "/rustc/abc123/library/core/src/option.rs",
                "/rustc/abc123/library/core/src/option.rs",
            ),
            (
                "/home/anna/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/tokio-1.40.0/src/task.rs",
                "tokio-1.40.0/src/task.rs",
            ),
            (
                "/home/anna/work/kourier/target/debug/build/kourier-qt-0a1b/out/gen.rs",
                "gen.rs",
            ),
        ] {
            assert_eq!(shown_file(file), shown);
            assert!(
                !panic_line(Some((file, 1, 1)), None).contains("anna"),
                "{file}"
            );
        }
    }

    const HOOK_CHILD_ENV: &str = "KOURIER_PANIC_HOOK_CHILD";
    const SECRET: &str = "secret-payload +380501234567";

    /// A panic hook is the process's, so the test runs in a child process (this test binary,
    /// asked for `panic_hook_child` alone) and cannot reach any other test.
    #[test]
    fn the_panic_hook_hands_over_a_line_without_the_message() {
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args(["--exact", "process::tests::panic_hook_child", "--nocapture"])
            .env(HOOK_CHILD_ENV, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stdout}\n{stderr}");
        assert!(
            stdout.contains("panic_hook_child ran"),
            "the child test did not run: {stdout}"
        );
        // The previous hook ran after ours: the default one prints the message to stderr.
        assert!(stderr.contains(SECRET), "{stderr}");
    }

    /// The body of the child process of
    /// [`the_panic_hook_hands_over_a_line_without_the_message`]; a no-op in the parent run.
    #[test]
    fn panic_hook_child() {
        use std::sync::Mutex;

        if std::env::var_os(HOOK_CHILD_ENV).is_none() {
            return;
        }
        static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
        fn sink(line: &str) {
            LINES.lock().unwrap().push(line.to_owned());
        }
        install_panic_hook(sink);

        assert!(std::panic::catch_unwind(|| panic!("{SECRET}")).is_err());
        // A thread whose name holds a whole id: the line passes the log's redaction.
        let id = "550e8400-e29b-41d4-a716-44665544ffff";
        std::thread::Builder::new()
            .name(id.to_owned())
            .spawn(|| panic!("{SECRET}"))
            .unwrap()
            .join()
            .unwrap_err();

        let lines = LINES.lock().unwrap();
        assert_eq!(lines.len(), 2, "{lines:?}");
        for line in lines.iter() {
            assert!(line.starts_with("panicked at "), "{line}");
            assert!(line.contains("process.rs:"), "{line}");
            assert!(
                !line.contains("secret") && !line.contains("380501234567"),
                "{line}"
            );
            assert!(!line.contains(id), "{line}");
            assert_eq!(line.lines().count(), 1, "{line}");
        }
        assert!(lines[1].contains("…ffff"), "{}", lines[1]);
        println!("panic_hook_child ran");
    }
}
