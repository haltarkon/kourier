//! The helper's network lock-down (TG-1b decision B-3; TG-1 decisions D3 H1(b)): what keeps TDLib
//! inside its gate even if its own proxy rule fails (a refused `addProxy`, a fallback path the
//! config patch missed). The helper binary applies it after it opened the library and before its
//! serve loop creates the first client; there is no switch that turns it off.
//!
//! Three steps, in this order, while the process has ONE thread (a thread that exists before
//! Landlock's `restrict_self` keeps its old rights, so a second thread is a failure):
//!
//! 1. `PR_SET_NO_NEW_PRIVS` (both steps below need it).
//! 2. A seccomp filter (classic BPF written here, synchronised to every thread). `socket` has an
//!    ALLOW-LIST of families: `AF_INET` and `AF_INET6` only as a plain TCP stream (`SOCK_STREAM`
//!    with protocol 0 or `IPPROTO_TCP`: `SOCK_DGRAM`, `SOCK_RAW`, `SOCK_SEQPACKET` and a protocol
//!    such as MPTCP or SCTP, which Landlock's TCP rules do not see, fail with `EPERM`, whatever
//!    `SOCK_NONBLOCK`/`SOCK_CLOEXEC` say), `AF_UNIX` and `AF_NETLINK` (glibc's resolver reads the
//!    routes through it); EVERY OTHER FAMILY (`AF_SMC`, which carries TCP past Landlock, `AF_RDS`,
//!    `AF_TIPC`, `AF_VSOCK`, `AF_PACKET`, ...) fails with `EPERM`, answered before the kernel
//!    looks at the family, so no protocol module is loaded for it. `listen` fails with `EPERM`
//!    (TDLib never listens, and an unbound `listen` would otherwise pick a TCP port by itself);
//!    `io_uring_setup` fails with `EPERM` (an io_uring could create a socket past the filter); a
//!    system call of another ABI (x32, i386 through `int 0x80`) fails with `EPERM`. Everything
//!    else is allowed, except the PATH-based metadata calls (K3, the security review's M1):
//!    `chmod`, `fchmodat`, `fchmodat2`, `chown`, `lchown`, `fchownat`, `setxattr`, `lsetxattr`,
//!    `removexattr`, `lremovexattr`, `setxattrat`, `removexattrat`, `utime`, `utimes`,
//!    `futimesat` and `utimensat` with a path fail with `EPERM` (the descriptor-based ones stay;
//!    see [`path_metadata_calls`]).
//! 3. Landlock (ABI 4 or later): TCP `connect` only to the gate's port, TCP `bind` never; from ABI
//!    6 on the domain is also scoped (abstract `AF_UNIX` sockets and signals of processes outside
//!    it are out of reach). Best effort: a kernel without it (or a container that hides it) is
//!    `unavailable`, and the hello says so; a kernel that offers it and then refuses a step is a
//!    failure. KNOWN LIMIT: a pathname `AF_UNIX` socket of a host daemon (such as
//!    systemd-resolved's varlink socket) stays reachable; TDLib resolves no names (its proxy is a
//!    literal 127.0.0.1) and has no code for it.
//! 4. The same ruleset handles EVERY filesystem right the ABI knows (TG-2b; ABI 4: 15 rights,
//!    ABI 5 and later: 16) and allows only: inside the account's `tdlib/` every right but
//!    EXECUTE, the special nodes, IOCTL_DEV and REFER (`TDLIB_DIR_RIGHTS`); inside its files
//!    directory the same plus REFER (`FILES_DIR_RIGHTS`); a few system files read only
//!    (`SYSTEM_FILES`, each optional). The directory rights come from READING TDLib (decision
//!    P-2): its later storage steps (the binlog's reindex and torn-tail truncate, SQLite's journal
//!    and WAL, the handshake's writes) are past what the offline measurement reaches, and a
//!    denial there would be fatal; pre3's chunk A4 measures them under these rules. The rules name
//!    the INODES opened (`O_PATH`) before `restrict_self`: the directories are made before the
//!    spawn and replaced only after the helper stopped. Landlock cannot restrict `stat`, `access`,
//!    `chdir`, `flock`/`fcntl` locks, `chmod`, `chown`, `utime` or extended attributes: the
//!    seccomp filter above answers the path-based metadata calls `EPERM` (K3).
//!
//! WHAT STAYS OPEN, said plainly: `stat`, `access`, `chdir` and path resolution anywhere (no
//! contents, but the names and sizes of what the uid can reach); `ioctl` on non-device files and
//! `fcntl`; descriptors opened BEFORE `restrict_self` (fd 3, the library's mapping); disk filling
//! (no quota inside the two directories); the descriptor-based metadata calls on a descriptor the
//! helper already holds; and, in a DEBUG helper (dumpable) under `kernel.yama.ptrace_scope` 0, the
//! same uid's `ptrace` and `/proc/<pid>/mem` (the PC runs scope 1: only ancestors).
//!
//! A required account directory that is missing, a symlink, not this uid's or not 0700 fails with
//! [`LockDownError::Paths`] before anything is restricted. A failure of any step ends the helper
//! with `F lock-down-failed` before any client exists.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

/// What the lock-down reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Landlock {
    /// Landlock restricts TCP to the gate's port.
    On,
    /// The kernel offers no Landlock with network rules (ABI < 4, or the call is not allowed).
    Unavailable,
}

impl Landlock {
    /// The hello's `landlock` value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Why the lock-down failed (a fixed step, never more).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockDownError {
    /// The process has more than one thread.
    Threads,
    /// `PR_SET_NO_NEW_PRIVS` failed.
    NoNewPrivs,
    /// The seccomp filter was not installed (on every thread).
    Seccomp,
    /// Landlock is offered but a ruleset, its rule or `restrict_self` failed.
    Landlock,
    /// A required account directory does not open as a directory of this uid with mode 0700
    /// (missing, a symlink, another owner, wider rights). Checked before anything is restricted.
    Paths,
}

/// The account's two directories, the only places the helper's TDLib may write (TG-2b): its
/// `database_directory` (`<account>/tdlib`) and its `files_directory` (`<files root>/<uuid>`). Both
/// exist before the helper starts and are not replaced while it runs (the rules name the INODES
/// opened here).
#[derive(Clone, Copy, Debug)]
pub struct FsPaths<'a> {
    pub database_dir: &'a Path,
    pub files_dir: &'a Path,
}

/// Applies the whole lock-down for a gate on `gate_port` and the account's directories `dirs`.
/// Irreversible for this process and every process it starts.
pub fn apply(gate_port: u16, dirs: &FsPaths<'_>) -> Result<Landlock, LockDownError> {
    if thread_count() != Some(1) {
        return Err(LockDownError::Threads);
    }
    apply_on_this_thread(gate_port, dirs)
}

/// [`apply`] without the one-thread check, for the lock-down test's child (libtest runs a test on
/// a thread of its own): seccomp reaches every thread (`TSYNC`), Landlock this thread and what it
/// starts. The helper binary calls [`apply`].
#[doc(hidden)]
pub fn apply_on_this_thread(gate_port: u16, dirs: &FsPaths<'_>) -> Result<Landlock, LockDownError> {
    let system: Vec<&str> = SYSTEM_FILES.iter().map(|(path, _)| *path).collect();
    apply_with_system_files(gate_port, dirs, &system)
}

/// [`apply_on_this_thread`] with other read-only system files than `SYSTEM_FILES`: the lock-down
/// test's child names a file of its own, which it may write without Landlock (the real system
/// files are root's, so the kernel's own permission check would answer first).
#[doc(hidden)]
pub fn apply_with_system_files(
    gate_port: u16,
    dirs: &FsPaths<'_>,
    system_files: &[&str],
) -> Result<Landlock, LockDownError> {
    // The required directories first: a failure here has restricted nothing.
    let rules = FsRules::open(dirs, system_files)?;
    // SAFETY: prctl with integer arguments only changes a flag of this process.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(LockDownError::NoNewPrivs);
    }
    install_seccomp().map_err(|_| LockDownError::Seccomp)?;
    landlock(gate_port, &rules)
}

/// The number of this process's threads, from `/proc/self/task`.
fn thread_count() -> Option<usize> {
    std::fs::read_dir("/proc/self/task")
        .ok()
        .map(|d| d.filter(|e| e.is_ok()).count())
}

// ---- seccomp ----------------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc000_00b7;

/// x32's system call bit: no x86-64 call has it (on aarch64 no call number comes near it).
const X32_BIT: u32 = 0x4000_0000;
/// The socket type without `SOCK_NONBLOCK` and `SOCK_CLOEXEC` (`SOCK_TYPE_MASK`, `<linux/net.h>`).
const SOCK_TYPE_MASK: u32 = 0xf;

// Offsets in `struct seccomp_data` (`<linux/seccomp.h>`): nr, arch, instruction pointer, args[6].
// An argument is 64 bits; the low word comes first on these little-endian targets, and `socket`'s
// three arguments are `int`s, so the kernel itself reads only the low word.
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARG0: u32 = 16;
const OFF_ARG1: u32 = 24;
const OFF_ARG2: u32 = 32;
/// The high word of argument 1 (a pointer: `utimensat`'s path).
const OFF_ARG1_HIGH: u32 = OFF_ARG1 + 4;

const LD_W_ABS: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
const JEQ_K: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
const JGE_K: u16 = (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16;
const AND_K: u16 = (libc::BPF_ALU | libc::BPF_AND | libc::BPF_K) as u16;
const RET_K: u16 = (libc::BPF_RET | libc::BPF_K) as u16;

const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
const DENY: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;

const fn op(code: u16, jt: u8, jf: u8, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// `setxattrat` and `removexattrat` (kernel 6.13, the same numbers on x86-64 and aarch64): the
/// path-based extended-attribute calls of newer kernels, not yet in the `libc` crate's table.
const SYS_SETXATTRAT: u32 = 463;
const SYS_REMOVEXATTRAT: u32 = 466;

/// The system calls the filter answers `EPERM` (K3, the security review's M1), besides `socket`'s
/// rules, `listen` and `io_uring_setup`: the PATH-based metadata calls. Landlock cannot restrict
/// them, and a compromised TDLib could otherwise `chmod 0644 ~/.ssh/id_ed25519`, grant another uid
/// an ACL (`system.posix_acl_access`) on Kourier's other databases or make `~/.bashrc` world
/// writable: the one way out of the rules that needs no kernel bug. The descriptor-based calls
/// stay (`fchmod`, `fchown`, `fsetxattr`, `fremovexattr`; SQLite uses `fchmod`): a descriptor
/// outside the rules cannot be opened except `O_PATH`, where they fail `EBADF`. `utimensat` is
/// answered only when its path argument is not NULL (see [`filter`]): glibc's `futimens`, which
/// TDLib uses (`Stat.cpp:151`), passes NULL. The measurement (A0, A3.3, A4: no helper call of
/// these) shows TDLib makes none of them. The legacy numbers (`chmod`, `chown`, `lchown`,
/// `utime`, `utimes`, `futimesat`) exist on x86-64 only; the filter takes the others on every
/// architecture it supports. RE-DERIVE at every pin bump and kernel table change (LC-20).
fn path_metadata_calls() -> Vec<u32> {
    #[cfg_attr(not(target_arch = "x86_64"), allow(unused_mut))]
    let mut calls = vec![
        libc::SYS_fchmodat as u32,
        libc::SYS_fchmodat2 as u32,
        libc::SYS_fchownat as u32,
        libc::SYS_setxattr as u32,
        libc::SYS_lsetxattr as u32,
        libc::SYS_removexattr as u32,
        libc::SYS_lremovexattr as u32,
        SYS_SETXATTRAT,
        SYS_REMOVEXATTRAT,
    ];
    #[cfg(target_arch = "x86_64")]
    calls.extend([
        libc::SYS_chmod as u32,
        libc::SYS_chown as u32,
        libc::SYS_lchown as u32,
        libc::SYS_utime as u32,
        libc::SYS_utimes as u32,
        libc::SYS_futimesat as u32,
    ]);
    calls
}

/// Where a filter instruction jumps: on to the next one, or to a named place.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dest {
    Next,
    To(Mark),
}

/// The named places of the filter.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    /// `socket`'s family is `AF_INET` or `AF_INET6`: the type and protocol checks.
    Inet,
    /// `utimensat`: its path argument is looked at.
    Utimensat,
    Allow,
    Deny,
}

/// One piece of the program: an instruction with symbolic jumps, or the place a [`Mark`] names.
enum Piece {
    Ins(u16, u32, Dest, Dest),
    At(Mark),
}

/// Resolves the symbolic jumps (relative to the next instruction, as BPF wants them). Panics on a
/// jump back, a mark that is not placed or a jump over 255: the program is fixed code, and the
/// unit tests build it.
fn assemble(pieces: &[Piece]) -> Vec<libc::sock_filter> {
    let mut at = Vec::new();
    let mut n = 0usize;
    for piece in pieces {
        match piece {
            Piece::Ins(..) => n += 1,
            Piece::At(mark) => at.push((*mark, n)),
        }
    }
    let target = |mark: Mark| {
        at.iter()
            .find(|(m, _)| *m == mark)
            .expect("a placed mark")
            .1
    };
    let mut out = Vec::with_capacity(n);
    for piece in pieces {
        let Piece::Ins(code, k, jt, jf) = piece else {
            continue;
        };
        let here = out.len();
        let offset = |d: Dest| -> u8 {
            match d {
                Dest::Next => 0,
                Dest::To(m) => {
                    u8::try_from(target(m) - here - 1).expect("a forward jump under 256")
                }
            }
        };
        out.push(op(*code, offset(*jt), offset(*jf), *k));
    }
    out
}

/// The filter, in order: the architecture (any other, and x32's call numbers, are `EPERM`); the
/// calls answered `EPERM` whatever their arguments (`io_uring_setup`, `listen`, the path-based
/// metadata calls); `utimensat` with a path (argument 1, both 32-bit halves, not NULL) `EPERM`,
/// without one (`futimens`) allowed; `socket` by its family, type and protocol; every other call
/// allowed.
fn filter() -> Vec<libc::sock_filter> {
    use Dest::{Next, To};
    use Mark::{Allow, Deny, Inet, Utimensat};
    use Piece::{At, Ins};
    let mut p = vec![
        Ins(LD_W_ABS, OFF_ARCH, Next, Next),
        Ins(JEQ_K, AUDIT_ARCH, Next, To(Deny)),
        Ins(LD_W_ABS, OFF_NR, Next, Next),
        Ins(JGE_K, X32_BIT, To(Deny), Next),
        Ins(JEQ_K, libc::SYS_io_uring_setup as u32, To(Deny), Next),
        Ins(JEQ_K, libc::SYS_listen as u32, To(Deny), Next),
    ];
    for nr in path_metadata_calls() {
        p.push(Ins(JEQ_K, nr, To(Deny), Next));
    }
    p.extend([
        Ins(JEQ_K, libc::SYS_utimensat as u32, To(Utimensat), Next),
        Ins(JEQ_K, libc::SYS_socket as u32, Next, To(Allow)),
        Ins(LD_W_ABS, OFF_ARG0, Next, Next),
        Ins(JEQ_K, libc::AF_INET as u32, To(Inet), Next),
        Ins(JEQ_K, libc::AF_INET6 as u32, To(Inet), Next),
        Ins(JEQ_K, libc::AF_UNIX as u32, To(Allow), Next),
        Ins(JEQ_K, libc::AF_NETLINK as u32, To(Allow), To(Deny)),
        At(Inet),
        Ins(LD_W_ABS, OFF_ARG1, Next, Next),
        Ins(AND_K, SOCK_TYPE_MASK, Next, Next),
        Ins(JEQ_K, libc::SOCK_STREAM as u32, Next, To(Deny)),
        Ins(LD_W_ABS, OFF_ARG2, Next, Next),
        Ins(JEQ_K, 0, To(Allow), Next),
        Ins(JEQ_K, libc::IPPROTO_TCP as u32, To(Allow), To(Deny)),
        // utimensat(dirfd, pathname, times, flags): `pathname` NULL (both halves) is `futimens`.
        At(Utimensat),
        Ins(LD_W_ABS, OFF_ARG1, Next, Next),
        Ins(JEQ_K, 0, Next, To(Deny)),
        Ins(LD_W_ABS, OFF_ARG1_HIGH, Next, Next),
        Ins(JEQ_K, 0, To(Allow), To(Deny)),
        At(Allow),
        Ins(RET_K, ALLOW, Next, Next),
        At(Deny),
        Ins(RET_K, DENY, Next, Next),
    ]);
    assemble(&p)
}

fn install_seccomp() -> io::Result<()> {
    let prog = filter();
    let fprog = libc::sock_fprog {
        len: prog.len() as u16,
        filter: prog.as_ptr().cast_mut(),
    };
    // SAFETY: seccomp(SET_MODE_FILTER, TSYNC, &fprog): the kernel copies the program before it
    // returns; `prog` outlives the call. TSYNC returns a thread id (> 0) when a thread cannot
    // be synchronised, 0 on success, -1 on an error.
    let r = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_TSYNC,
            &fprog as *const libc::sock_fprog,
        )
    };
    match r {
        0 => Ok(()),
        r if r > 0 => Err(io::Error::other("a thread was not synchronised")),
        _ => Err(io::Error::last_os_error()),
    }
}

// ---- Landlock ---------------------------------------------------------------------------------

/// `LANDLOCK_CREATE_RULESET_VERSION`.
const CREATE_RULESET_VERSION: u32 = 1 << 0;
/// `LANDLOCK_ACCESS_NET_BIND_TCP`, `LANDLOCK_ACCESS_NET_CONNECT_TCP` (ABI 4).
const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;
/// `LANDLOCK_RULE_NET_PORT`.
const RULE_NET_PORT: libc::c_int = 2;
/// The first ABI with network rules.
const NET_ABI: libc::c_long = 4;

/// `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`, `LANDLOCK_SCOPE_SIGNAL` (ABI 6).
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;
/// The first ABI with scopes.
const SCOPE_ABI: libc::c_long = 6;

// Filesystem rights (`LANDLOCK_ACCESS_FS_*`): bits 0-12 since ABI 1, REFER (13) ABI 2, TRUNCATE
// (14) ABI 3, IOCTL_DEV (15) ABI 5; nothing new up to ABI 8 (kernel 7.0's header).
#[cfg_attr(not(test), allow(dead_code))] // named for the tests and the reader
pub(crate) const FS_EXECUTE: u64 = 1 << 0;
pub(crate) const FS_WRITE_FILE: u64 = 1 << 1;
pub(crate) const FS_READ_FILE: u64 = 1 << 2;
pub(crate) const FS_READ_DIR: u64 = 1 << 3;
pub(crate) const FS_REMOVE_DIR: u64 = 1 << 4;
pub(crate) const FS_REMOVE_FILE: u64 = 1 << 5;
#[cfg_attr(not(test), allow(dead_code))] // named for the tests and the reader
pub(crate) const FS_MAKE_CHAR: u64 = 1 << 6;
pub(crate) const FS_MAKE_DIR: u64 = 1 << 7;
pub(crate) const FS_MAKE_REG: u64 = 1 << 8;
#[cfg_attr(not(test), allow(dead_code))] // named for the tests and the reader
pub(crate) const FS_MAKE_SOCK: u64 = 1 << 9;
#[cfg_attr(not(test), allow(dead_code))] // named for the tests and the reader
pub(crate) const FS_MAKE_FIFO: u64 = 1 << 10;
#[cfg_attr(not(test), allow(dead_code))] // named for the tests and the reader
pub(crate) const FS_MAKE_BLOCK: u64 = 1 << 11;
#[cfg_attr(not(test), allow(dead_code))] // named for the tests and the reader
pub(crate) const FS_MAKE_SYM: u64 = 1 << 12;
pub(crate) const FS_REFER: u64 = 1 << 13;
pub(crate) const FS_TRUNCATE: u64 = 1 << 14;
pub(crate) const FS_IOCTL_DEV: u64 = 1 << 15;
/// The first ABI with `IOCTL_DEV`.
const IOCTL_DEV_ABI: libc::c_long = 5;
/// The rights a rule on a FILE (not a directory) may carry; any other right there is EINVAL.
#[cfg_attr(not(test), allow(dead_code))] // named for the tests and the reader
pub(crate) const FS_FILE_RIGHTS: u64 =
    FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE | FS_IOCTL_DEV;

/// Inside `<account>/tdlib` (decision P-2: from READING TDLib, not from the measurement, because
/// its storage failures past the handshake are fatal: the binlog's truncate after a torn tail, its
/// reindex through `td.binlog.new`, SQLite's journal and WAL files): every right but EXECUTE, the
/// special nodes (symlink, FIFO, socket, devices), IOCTL_DEV and REFER. A rename inside the one
/// directory needs no REFER.
pub(crate) const TDLIB_DIR_RIGHTS: u64 = FS_WRITE_FILE
    | FS_READ_FILE
    | FS_READ_DIR
    | FS_REMOVE_DIR
    | FS_REMOVE_FILE
    | FS_MAKE_DIR
    | FS_MAKE_REG
    | FS_TRUNCATE;
/// Inside the files directory: the same plus REFER (TDLib's file manager makes a directory per file
/// type and renames a finished temporary file across them, `FileLoaderUtils.cpp`).
pub(crate) const FILES_DIR_RIGHTS: u64 = TDLIB_DIR_RIGHTS | FS_REFER;
/// A system file TDLib reads after the lock-down: read only.
pub(crate) const SYSTEM_FILE_RIGHTS: u64 = FS_READ_FILE;

/// The filesystem rights this ABI knows, all of them handled (restricted unless a rule allows
/// them). Below ABI 4 the whole Landlock part is `unavailable` and nothing is handled.
pub(crate) fn handled_fs(abi: libc::c_long) -> u64 {
    if abi < NET_ABI {
        0
    } else if abi < IOCTL_DEV_ABI {
        FS_IOCTL_DEV - 1
    } else {
        (FS_IOCTL_DEV << 1) - 1
    }
}

/// Never allowed anywhere: TDLib starts nothing and makes no special node.
#[cfg_attr(not(test), allow(dead_code))] // named for the tests and the reader
pub(crate) const FS_NEVER: u64 = FS_EXECUTE
    | FS_MAKE_SYM
    | FS_MAKE_FIFO
    | FS_MAKE_SOCK
    | FS_MAKE_CHAR
    | FS_MAKE_BLOCK
    | FS_IOCTL_DEV;

/// The system files TDLib (or glibc, OpenSSL on its behalf) reads after the lock-down, each read
/// only and OPTIONAL (a system without the file skips its rule). From the measurement A0
/// (`target/tg/tg-2b-measure.md`, 2026-10-26: the builder, kernel 7.0, Landlock ABI 8, six
/// offline cases, every `openat` after `landlock_restrict_self`), re-measured at the pin bump to
/// TDLib 1.8.69 with patches 0001 and 0002 (LC-20, 2026-11-01, `target/tg/lc20/strace-measure.md`:
/// the same six cases and the fake DC's login, logout and kill-restart): the same five files,
/// no new one. Opened FOLLOWING symlinks, so the rule names the file a link points to
/// (`/etc/localtime`, Ubuntu's `/usr/lib/ssl/openssl.cnf`). RE-MEASURE at every pin bump and
/// when downloads start (T6). Everything else outside the two directories is denied: the
/// resolver's files, `/etc/os-release`, `/proc/self/*` (none was opened after the cutoff).
pub(crate) const SYSTEM_FILES: &[(&str, &str)] = &[
    (
        "/dev/urandom",
        "OpenSSL's random seed, opened once per process",
    ),
    ("/etc/localtime", "glibc's time zone (TDLib's UTC offset)"),
    (
        "/proc/sys/crypto/fips_enabled",
        "OpenSSL's FIPS check at its init (absent in the builder's container, present on a host)",
    ),
    (
        "/sys/devices/system/cpu/online",
        "glibc's CPU count for TDLib's thread pool",
    ),
    (
        "/usr/lib/ssl/openssl.cnf",
        "OpenSSL's configuration, read at its init",
    ),
];

/// `LANDLOCK_RULE_PATH_BENEATH`.
const RULE_PATH_BENEATH: libc::c_int = 1;

/// `struct landlock_path_beneath_attr`: PACKED in the kernel's header (12 bytes).
#[repr(C, packed)]
pub(crate) struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// One opened rule: an `O_PATH` descriptor (closed on drop) and its rights.
struct FsRule {
    fd: libc::c_int,
    rights: u64,
}

impl Drop for FsRule {
    fn drop(&mut self) {
        // SAFETY: closes a descriptor this rule owns.
        unsafe { libc::close(self.fd) };
    }
}

/// The opened rules: the two directories (required) and the system files that exist.
struct FsRules(Vec<FsRule>);

impl FsRules {
    fn open(dirs: &FsPaths<'_>, system_files: &[&str]) -> Result<Self, LockDownError> {
        let mut rules = vec![
            FsRule {
                fd: open_account_dir(dirs.database_dir).ok_or(LockDownError::Paths)?,
                rights: TDLIB_DIR_RIGHTS,
            },
            FsRule {
                fd: open_account_dir(dirs.files_dir).ok_or(LockDownError::Paths)?,
                rights: FILES_DIR_RIGHTS,
            },
        ];
        rules.extend(system_files.iter().filter_map(|path| {
            open_system_file(Path::new(path)).map(|fd| FsRule {
                fd,
                rights: SYSTEM_FILE_RIGHTS,
            })
        }));
        Ok(Self(rules))
    }
}

/// `open(path, O_PATH | O_CLOEXEC | extra)`.
fn open_path(path: &Path, extra: libc::c_int) -> Option<libc::c_int> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: a NUL-terminated path; open returns a new descriptor or -1.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC | extra) };
    (fd >= 0).then_some(fd)
}

/// A system file's rule (K8, the security review's L2): opened following symlinks, kept only when
/// the descriptor's `fstat` says a regular file or a character device (`/dev/urandom`). A path
/// that is, or links to, a DIRECTORY (an `/etc/localtime` link into a zoneinfo directory by
/// mistake, a bind mount) would turn the file rule into a directory rule and let TDLib read the
/// whole tree beneath; it is skipped like a missing file.
fn open_system_file(path: &Path) -> Option<libc::c_int> {
    let fd = open_path(path, 0)?;
    // SAFETY: fstat fills `st` for a descriptor of this process.
    let ok = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(fd, &mut st) == 0
            && matches!(st.st_mode & libc::S_IFMT, libc::S_IFREG | libc::S_IFCHR)
    };
    if ok {
        Some(fd)
    } else {
        // SAFETY: closes the descriptor opened above.
        unsafe { libc::close(fd) };
        None
    }
}

/// An account directory: absolute, opened without following a final symlink, a directory of this
/// uid with mode 0700 (L-b).
fn open_account_dir(path: &Path) -> Option<libc::c_int> {
    if !path.is_absolute() {
        return None;
    }
    let fd = open_path(path, libc::O_DIRECTORY | libc::O_NOFOLLOW)?;
    // SAFETY: fstat fills `st` for a descriptor of this process.
    let ok = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(fd, &mut st) == 0
            && st.st_mode & libc::S_IFMT == libc::S_IFDIR
            && st.st_uid == libc::geteuid()
            && st.st_mode & 0o7777 == 0o700
    };
    if ok {
        Some(fd)
    } else {
        // SAFETY: closes the descriptor opened above.
        unsafe { libc::close(fd) };
        None
    }
}

/// `struct landlock_ruleset_attr` up to its scope field (ABI 6; the kernel takes a shorter size
/// than its own and treats the missing fields as zero, so ABI 4 and 5 are given the first 16
/// bytes).
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

/// The size of the ruleset attribute up to the network field (ABI 4 and 5).
const ATTR_NET_SIZE: usize = 2 * std::mem::size_of::<u64>();

/// `struct landlock_net_port_attr`.
#[repr(C)]
struct NetPortAttr {
    allowed_access: u64,
    port: u64,
}

/// The Landlock ABI this kernel offers, or `None` (no Landlock, or the call is refused).
pub fn landlock_abi() -> Option<libc::c_long> {
    // SAFETY: the version query takes a NULL attribute and size 0 and only returns a number.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    (abi > 0).then_some(abi)
}

fn landlock(gate_port: u16, rules: &FsRules) -> Result<Landlock, LockDownError> {
    let Some(abi) = landlock_abi().filter(|abi| *abi >= NET_ABI) else {
        return Ok(Landlock::Unavailable);
    };
    // From ABI 6 the domain is scoped too: no abstract AF_UNIX socket and no process outside it.
    let scoped = abi >= SCOPE_ABI;
    let handled = handled_fs(abi);
    let attr = RulesetAttr {
        handled_access_fs: handled,
        handled_access_net: NET_BIND_TCP | NET_CONNECT_TCP,
        scoped: if scoped {
            SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL
        } else {
            0
        },
    };
    let size = if scoped {
        std::mem::size_of::<RulesetAttr>()
    } else {
        ATTR_NET_SIZE
    };
    // SAFETY: the kernel reads `size` bytes of `attr` (at most its size) and returns a new
    // descriptor or -1.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            size,
            0u32,
        )
    };
    if fd < 0 {
        return Err(LockDownError::Landlock);
    }
    let fd = fd as libc::c_int;
    let rule = NetPortAttr {
        allowed_access: NET_CONNECT_TCP,
        port: u64::from(gate_port),
    };
    // SAFETY: `fd` is the ruleset just made; the kernel reads `rule` during the call; the
    // descriptor is closed once the ruleset is enforced (the domain keeps its own reference).
    let ok = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            fd,
            RULE_NET_PORT,
            &rule as *const NetPortAttr,
            0u32,
        ) == 0
            && rules.0.iter().all(|r| {
                let attr = PathBeneathAttr {
                    allowed_access: r.rights & handled,
                    parent_fd: r.fd,
                };
                libc::syscall(
                    libc::SYS_landlock_add_rule,
                    fd,
                    RULE_PATH_BENEATH,
                    &attr as *const PathBeneathAttr,
                    0u32,
                ) == 0
            })
            && libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) == 0
    };
    // SAFETY: closes the ruleset descriptor this function owns.
    unsafe { libc::close(fd) };
    if ok {
        Ok(Landlock::On)
    } else {
        Err(LockDownError::Landlock)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_right_the_abi_knows_is_handled() {
        assert_eq!(handled_fs(3), 0);
        assert_eq!(handled_fs(4), 0x7fff, "ABI 4: EXECUTE .. TRUNCATE");
        assert_eq!(handled_fs(4) & FS_IOCTL_DEV, 0, "IOCTL_DEV is ABI 5");
        for abi in 5..=8 {
            assert_eq!(handled_fs(abi), 0xffff, "ABI {abi}");
        }
    }

    #[test]
    fn the_allowed_sets_are_the_decided_ones() {
        let never = FS_NEVER;
        for set in [TDLIB_DIR_RIGHTS, FILES_DIR_RIGHTS, SYSTEM_FILE_RIGHTS] {
            assert_eq!(set & never, 0, "{set:#x} holds a right allowed nowhere");
            for abi in 4..=8 {
                assert_eq!(set & handled_fs(abi), set, "ABI {abi}: {set:#x} is handled");
            }
        }
        // P-2: everything else inside both directories; REFER only in the files directory.
        assert_eq!(TDLIB_DIR_RIGHTS, 0xffff & !never & !FS_REFER);
        assert_eq!(FILES_DIR_RIGHTS, TDLIB_DIR_RIGHTS | FS_REFER);
        // A file rule holds only file rights (the kernel answers EINVAL otherwise).
        assert_eq!(SYSTEM_FILE_RIGHTS & !FS_FILE_RIGHTS, 0);
        assert_eq!(SYSTEM_FILE_RIGHTS, FS_READ_FILE);
    }

    #[test]
    fn the_path_beneath_attribute_is_packed() {
        assert_eq!(std::mem::size_of::<PathBeneathAttr>(), 12);
    }

    #[test]
    fn every_system_file_is_absolute_and_has_a_reason() {
        for (path, why) in SYSTEM_FILES {
            assert!(path.starts_with('/') && !why.is_empty(), "{path}");
        }
    }

    /// Runs the classic-BPF program on a `seccomp_data` made of these words (the kernel's
    /// semantics for the five opcodes the filter uses; a return is the verdict).
    fn run(prog: &[libc::sock_filter], arch: u32, nr: u32, args: [u64; 6]) -> u32 {
        let load = |off: u32| -> u32 {
            match off {
                OFF_NR => nr,
                OFF_ARCH => arch,
                o if (OFF_ARG0..OFF_ARG0 + 48).contains(&o) => {
                    let i = ((o - OFF_ARG0) / 8) as usize;
                    let half = (o - OFF_ARG0) % 8;
                    (args[i] >> (half * 8)) as u32
                }
                _ => panic!("the filter reads offset {off}"),
            }
        };
        let (mut a, mut pc) = (0u32, 0usize);
        loop {
            let ins = &prog[pc];
            pc += 1;
            match ins.code {
                LD_W_ABS => a = load(ins.k),
                AND_K => a &= ins.k,
                JEQ_K => pc += usize::from(if a == ins.k { ins.jt } else { ins.jf }),
                JGE_K => pc += usize::from(if a >= ins.k { ins.jt } else { ins.jf }),
                RET_K => return ins.k,
                c => panic!("opcode {c:#x} is not one the filter uses"),
            }
        }
    }

    /// K3 (review M1): every path-based metadata call is `EPERM` whatever its arguments, `utimensat`
    /// only with a path (either 32-bit half of argument 1), and the descriptor-based calls, the
    /// reads and writes TDLib and SQLite make, stay allowed.
    #[test]
    fn the_path_based_metadata_calls_are_eperm_and_the_descriptor_based_ones_stay() {
        let f = filter();
        let call = |nr: i64, args: [u64; 6]| run(&f, AUDIT_ARCH, nr as u32, args);
        let none = [0u64; 6];
        for nr in path_metadata_calls() {
            assert_eq!(call(nr as i64, none), DENY, "call {nr} is allowed");
            assert_eq!(call(nr as i64, [1; 6]), DENY, "call {nr} is allowed");
        }
        // The calls the review names, one by one (x86-64 numbers from the libc crate).
        #[cfg(target_arch = "x86_64")]
        for nr in [
            libc::SYS_chmod,
            libc::SYS_fchmodat,
            libc::SYS_fchmodat2,
            libc::SYS_chown,
            libc::SYS_lchown,
            libc::SYS_fchownat,
            libc::SYS_setxattr,
            libc::SYS_lsetxattr,
            libc::SYS_removexattr,
            libc::SYS_lremovexattr,
            libc::SYS_utime,
            libc::SYS_utimes,
            libc::SYS_futimesat,
        ] {
            assert_eq!(call(nr, none), DENY, "call {nr}");
        }
        assert_eq!(call(SYS_SETXATTRAT.into(), none), DENY, "setxattrat");
        assert_eq!(call(SYS_REMOVEXATTRAT.into(), none), DENY, "removexattrat");
        // `utimensat`: a path in either half of argument 1 is EPERM, no path (`futimens`) is not.
        let utimensat = libc::SYS_utimensat;
        assert_eq!(call(utimensat, none), ALLOW, "futimens (a NULL path)");
        assert_eq!(
            call(utimensat, [3, 0, 7, 0, 0, 0]),
            ALLOW,
            "futimens on fd 3"
        );
        assert_eq!(
            call(utimensat, [-100i64 as u64, 0x7fff_0000_1000, 0, 0, 0, 0]),
            DENY
        );
        assert_eq!(
            call(utimensat, [0, 0x1000, 0, 0, 0, 0]),
            DENY,
            "low half only"
        );
        assert_eq!(
            call(utimensat, [0, 1 << 32, 0, 0, 0, 0]),
            DENY,
            "high half only"
        );
        // What stays: the descriptor-based calls and the everyday ones.
        for (what, nr) in [
            ("fchmod", libc::SYS_fchmod),
            ("fchown", libc::SYS_fchown),
            ("fsetxattr", libc::SYS_fsetxattr),
            ("fremovexattr", libc::SYS_fremovexattr),
            ("read", libc::SYS_read),
            ("write", libc::SYS_write),
            ("openat", libc::SYS_openat),
            ("renameat", libc::SYS_renameat),
            ("ftruncate", libc::SYS_ftruncate),
            ("fsync", libc::SYS_fsync),
            ("connect", libc::SYS_connect),
        ] {
            assert_eq!(call(nr, none), ALLOW, "{what} is refused");
        }
        // The earlier rules still hold.
        for nr in [libc::SYS_listen, libc::SYS_io_uring_setup] {
            assert_eq!(call(nr, none), DENY);
        }
        assert_eq!(run(&f, AUDIT_ARCH + 1, libc::SYS_read as u32, none), DENY);
        assert_eq!(
            run(&f, AUDIT_ARCH, libc::SYS_read as u32 | X32_BIT, none),
            DENY
        );
        let socket = libc::SYS_socket;
        let tcp = [libc::AF_INET as u64, libc::SOCK_STREAM as u64, 0, 0, 0, 0];
        assert_eq!(call(socket, tcp), ALLOW);
        let udp = [libc::AF_INET as u64, libc::SOCK_DGRAM as u64, 0, 0, 0, 0];
        assert_eq!(call(socket, udp), DENY);
        let smc = [43, libc::SOCK_STREAM as u64, 0, 0, 0, 0];
        assert_eq!(call(socket, smc), DENY);
    }

    /// K8 (review L2): a "system file" that is a directory, or links to one, gets no rule.
    #[test]
    fn a_system_file_that_is_a_directory_is_skipped() {
        let dir = std::env::temp_dir().join(format!("k8-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let link = dir.with_extension("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&dir, &link).expect("link");
        let file = dir.join("file");
        std::fs::write(&file, b"x").expect("file");
        for dir_like in [dir.as_path(), link.as_path()] {
            let fd = open_system_file(dir_like);
            if let Some(fd) = fd {
                // SAFETY: closes the descriptor just opened.
                unsafe { libc::close(fd) };
            }
            assert!(fd.is_none(), "a directory got a file rule");
        }
        for ok in [file.as_path(), Path::new("/dev/null")] {
            let fd = open_system_file(ok).expect("a regular file or a device keeps its rule");
            // SAFETY: closes the descriptor just opened.
            unsafe { libc::close(fd) };
        }
        assert!(open_system_file(&dir.join("missing")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&link);
    }

    /// Every jump of the filter lands on an instruction inside it, and the last two are the
    /// verdicts the jumps name.
    #[test]
    fn every_jump_lands_inside_the_filter() {
        let f = filter();
        for (i, ins) in f.iter().enumerate() {
            if ins.code & 0x07 == libc::BPF_JMP as u16 {
                for off in [ins.jt, ins.jf] {
                    assert!(
                        i + 1 + (off as usize) < f.len(),
                        "instruction {i} jumps out"
                    );
                }
            }
        }
        let last = f.len() - 1;
        assert_eq!((f[last - 1].code, f[last - 1].k), (RET_K, ALLOW));
        assert_eq!((f[last].code, f[last].k), (RET_K, DENY));
    }
}
