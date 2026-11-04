//! The helper's process hardening (decisions D3 H5), the app's rule (`kourier-app`'s
//! `process.rs`): release builds are non-dumpable, before the library is opened; a debug build
//! stays dumpable unless `KOURIER_DEV_NO_DUMP=1`, which the spawner passes through explicitly (it
//! clears the environment).
//!
//! The core limit is `{1, 1}` in EVERY build (pre3 K1, the security review's H1), NOT 0. A limit
//! of 0 does not stop a pipe `core_pattern`: systemd-coredump (`|/usr/lib/systemd/systemd-coredump
//! ... 9223372036854775808 ...`) hard-codes the limit slot to RLIM_INFINITY, and for a pipe the
//! kernel itself ignores `RLIMIT_CORE` except for the value 1, at which its recursion guard
//! refuses the dump ("RLIMIT_CORE is set to 1, aborting core"); a dump to a FILE needs at least
//! one page, so 1 stops that too. So a crash that is not TDLib's fatal path (a signal) leaves no
//! core of a process that holds the account's keys, with or without systemd-coredump, also while
//! the process stays dumpable (a debug build: a debugger can still attach). A NON-dumpable process
//! is never dumped at all, to a file, a pipe or a socket (the kernel's `coredump_skip`,
//! `fs/coredump.c`; `fs.suid_dumpable` only sets the flag of a process that changed credentials,
//! never one that cleared it): release builds were safe before K1, debug builds were not. When the
//! limit cannot be set (a hard limit of 0 from the launcher, such as limits.conf's `hard core 0`,
//! which no unprivileged process may raise to 1), the process becomes non-dumpable in EVERY build
//! (the K1 re-check), so the rule holds there too and only a debugger loses. No signal handlers: a
//! handler in a multi-threaded C++ process is no fix for a stack overflow.

/// The environment variable that makes a debug build non-dumpable (`1`).
pub const NO_DUMP_ENV: &str = "KOURIER_DEV_NO_DUMP";

/// Whether this process should deny dumps: every release build, a debug build when asked.
pub fn wants_no_dumps(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    !cfg!(debug_assertions) || env(NO_DUMP_ENV).is_some_and(|v| v == "1")
}

/// The core limit of every build: 1 byte, soft and hard (see the module docs for why not 0).
pub const CORE_LIMIT: libc::rlim_t = 1;

/// Whether [`deny_dumps`] makes the process non-dumpable: when asked ([`wants_no_dumps`]: every
/// release build), and in every build when the core limit is not `{1, 1}` afterwards.
pub fn must_be_non_dumpable(asked: bool, core_limited: bool) -> bool {
    asked || !core_limited
}

/// Sets the core limit to [`CORE_LIMIT`] (every build), reads it back, and makes the process
/// non-dumpable when [`must_be_non_dumpable`] says so. Returns whether the process is non-dumpable
/// afterwards.
pub fn deny_dumps() -> bool {
    let no_core = libc::rlimit {
        rlim_cur: CORE_LIMIT,
        rlim_max: CORE_LIMIT,
    };
    let mut now = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: setrlimit and getrlimit with pointers to local structs.
    let core_limited = unsafe {
        libc::setrlimit(libc::RLIMIT_CORE, &no_core);
        libc::getrlimit(libc::RLIMIT_CORE, &mut now) == 0
    } && (now.rlim_cur, now.rlim_max) == (CORE_LIMIT, CORE_LIMIT);
    if must_be_non_dumpable(wants_no_dumps(|k| std::env::var_os(k)), core_limited) {
        // SAFETY: prctl only changes a flag of this process, its unused arguments zero.
        unsafe {
            libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        }
    }
    // SAFETY: PR_GET_DUMPABLE reads a flag; the unused arguments are zero.
    unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// K1 (review H1): the core limit is {1, 1} also where the process stays dumpable (a debug build
    /// without `KOURIER_DEV_NO_DUMP`; the unit tests are such a build). A limit of 0 would leave a
    /// pipe `core_pattern` (systemd-coredump) free to store the memory. Lowers this test process's
    /// own limit, which nothing else here needs.
    #[cfg(debug_assertions)]
    #[test]
    fn the_core_limit_is_one_in_every_build() {
        if std::env::var_os(NO_DUMP_ENV).is_some() {
            return;
        }
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // A soft limit raised to the hard one first, so the limit afterwards is deny_dumps' work,
        // not the environment's. SAFETY: getrlimit and setrlimit with a local struct.
        unsafe {
            assert_eq!(libc::getrlimit(libc::RLIMIT_CORE, &mut lim), 0);
            lim.rlim_cur = lim.rlim_max;
            assert_eq!(libc::setrlimit(libc::RLIMIT_CORE, &lim), 0);
        }
        if lim.rlim_max == 0 {
            // A hard limit of 0 cannot be raised to 1 without privilege: nothing to read back.
            println!(
                "SKIPPED: the hard core limit is already 0 here (the fallback test covers that case)"
            );
            return;
        }
        assert!(!deny_dumps(), "a debug build stays dumpable");
        // SAFETY: getrlimit fills a local struct.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut lim) }, 0);
        assert_eq!(
            (lim.rlim_cur, lim.rlim_max),
            (1, 1),
            "the core limit must be 1 (0 does not stop systemd-coredump)"
        );
    }

    /// The K1 re-check: a process that cannot get the `{1, 1}` limit is made non-dumpable, in a
    /// debug build too.
    #[test]
    fn non_dumpable_when_asked_or_when_the_limit_is_not_one() {
        assert!(!must_be_non_dumpable(false, true));
        assert!(must_be_non_dumpable(true, true));
        assert!(must_be_non_dumpable(false, false));
        assert!(must_be_non_dumpable(true, false));
    }

    /// The K1 re-check, for real: a launcher's hard core limit of 0 (limits.conf's `hard core 0`),
    /// which an unprivileged process cannot raise to 1, leaves this debug build non-dumpable, so no
    /// pipe `core_pattern` receives it. Lowers this test process's own limit (nothing else here
    /// needs it; the other test then takes its SKIPPED branch) and clears its dumpable flag.
    #[cfg(debug_assertions)]
    #[test]
    fn a_core_limit_that_cannot_be_one_makes_the_process_non_dumpable() {
        if std::env::var_os(NO_DUMP_ENV).is_some() {
            return;
        }
        let zero = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: setrlimit with a local struct (lowering a limit needs no privilege).
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_CORE, &zero) }, 0);
        let non_dumpable = deny_dumps();
        let mut lim = zero;
        // SAFETY: getrlimit fills a local struct.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut lim) }, 0);
        if (lim.rlim_cur, lim.rlim_max) == (CORE_LIMIT, CORE_LIMIT) {
            println!("SKIPPED: this process may raise its hard core limit (privileged)");
            return;
        }
        assert!(
            non_dumpable,
            "without the {{1, 1}} core limit the process must be non-dumpable, in a debug build too"
        );
    }
}
