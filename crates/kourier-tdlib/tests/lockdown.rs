//! The helper's lock-down (TG-1b B-3) in a child process (the test binary re-executed: the
//! lock-down cannot be undone). No TDLib: the child applies `lockdown::apply_on_this_thread` the way
//! the helper binary applies `lockdown::apply`, then tries what TDLib could try. Every address is
//! loopback; nothing leaves the machine.
//!
//! Landlock is best effort: where the kernel (or a container's seccomp profile) does not offer it,
//! the TCP part is SKIPPED with a line saying so, and `KOURIER_REQUIRE_LANDLOCK=1` makes that a
//! failure. The seccomp part runs everywhere.

use std::io;
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};

use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::PathBuf;

use kourier_tdlib::lockdown::{self, FsPaths, Landlock, LockDownError};

// Socket families that carry traffic past the inet checks or that no TDLib path uses (`<linux/socket.h>`):
// spelled out so the test does not depend on which of them the libc crate names.
const AF_RDS: i32 = 21;
const AF_TIPC: i32 = 30;
const AF_VSOCK: i32 = 40;
const AF_SMC: i32 = 43;

const CHILD_ENV: &str = "KOURIER_TDLIB_LOCKDOWN_CHILD";
const I386_CHILD_ENV: &str = "KOURIER_TDLIB_LOCKDOWN_I386_CHILD";
const REQUIRE_ENV: &str = "KOURIER_REQUIRE_LANDLOCK";

/// `socket(domain, ty, proto)`: the errno of a refusal, or `None` when a socket was made (closed
/// at once).
fn socket_errno(domain: i32, ty: i32, proto: i32) -> Option<i32> {
    // SAFETY: socket returns a new descriptor or -1; a new descriptor is closed here.
    unsafe {
        let fd = libc::socket(domain, ty, proto);
        if fd >= 0 {
            libc::close(fd);
            None
        } else {
            io::Error::last_os_error().raw_os_error()
        }
    }
}

/// The root of a child's account directories, named by the PARENT's pid (the parent removes it when
/// the child ended).
fn dirs_root(parent: u32, what: &str) -> PathBuf {
    std::env::temp_dir().join(format!("kourier-lockdown-{parent}-{what}"))
}

/// Runs a child of this binary for `test` with `env` set, then removes its directories.
fn run_child(test: &str, env: &str, what: &str) -> std::process::Output {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(env, "1")
        .output()
        .expect("start the child");
    let _ = std::fs::remove_dir_all(dirs_root(std::process::id(), what));
    out
}

/// An account's two directories for a child: `<TMPDIR>/kourier-lockdown-<parent pid>-<what>/{tdlib,files}`,
/// both 0700 (what the supervisor makes before a spawn).
fn account_dirs(what: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = dirs_root(std::os::unix::process::parent_id(), what);
    let (db, files) = (root.join("tdlib"), root.join("files"));
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true).mode(0o700);
    for d in [&db, &files] {
        b.create(d).expect("an account directory");
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).expect("0700");
    }
    (root, db, files)
}

/// `prctl(option)` with no further argument (`PR_GET_NO_NEW_PRIVS`, `PR_GET_SECCOMP`): read
/// after the lock-down, when `/proc/self/status` is out of reach (TG-2b).
fn prctl_get(option: libc::c_int) -> libc::c_int {
    // SAFETY: a GET option only returns a number.
    unsafe { libc::prctl(option, 0, 0, 0, 0) }
}

fn status_field(name: &str) -> String {
    let status = std::fs::read_to_string("/proc/self/status").expect("read the status");
    status
        .lines()
        .find_map(|l| l.strip_prefix(name))
        .map(|v| v.trim().to_owned())
        .unwrap_or_default()
}

/// The child half: locks itself down for a "gate" on one loopback port and checks what remains.
#[test]
fn locked_down_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    // Both listeners exist before the lock-down (a bind afterwards is what Landlock refuses), and so
    // does an abstract Unix socket (the scope of ABI 6 keeps a locked-down process from it).
    let gate = TcpListener::bind("127.0.0.1:0").expect("bind the gate");
    let other = TcpListener::bind("127.0.0.1:0").expect("bind another port");
    let abstract_name = format!("kourier-tdlib-lockdown-{}", std::process::id());
    let abstract_addr = SocketAddr::from_abstract_name(abstract_name.as_bytes()).expect("name");
    let abstract_listener = UnixListener::bind_addr(&abstract_addr).expect("abstract socket");
    let (root, db, files) = account_dirs("child");
    let dirs = FsPaths {
        database_dir: &db,
        files_dir: &files,
    };
    // One read-only "system file" of the child's own (writable without Landlock), beside the two
    // account directories; and the files directory's temporary directory, made before.
    let system = root.join("system-file");
    std::fs::write(&system, b"system").expect("the system file");
    // K6: a file and an empty directory beside the account directories, outside every rule: what
    // the negative rows try to change.
    std::fs::write(root.join("victim"), b"victim").expect("root/victim");
    std::fs::create_dir(root.join("emptydir")).expect("root/emptydir");
    // K8: a "system file" that is a DIRECTORY holding a file. The rule must be skipped like a
    // missing file's, so the inner file stays unreadable.
    let sysdir = root.join("sysdir");
    std::fs::create_dir(&sysdir).expect("root/sysdir");
    std::fs::write(sysdir.join("inner"), b"inner").expect("root/sysdir/inner");
    std::fs::create_dir(files.join("temp")).expect("files/temp");
    std::fs::write(files.join("temp").join("part"), b"part").expect("a temporary file");
    let state = lockdown::apply_with_system_files(
        gate.local_addr().unwrap().port(),
        &dirs,
        &[
            system.to_str().expect("utf-8"),
            sysdir.to_str().expect("utf-8"),
        ],
    )
    .expect("the lock-down applies");
    assert_eq!(
        prctl_get(libc::PR_GET_NO_NEW_PRIVS),
        1,
        "no_new_privs is not set"
    );
    assert_eq!(prctl_get(libc::PR_GET_SECCOMP), 2, "no seccomp filter");

    // seccomp: no IPv4/IPv6 socket but a plain TCP stream, whatever the type flags say.
    let eperm = Some(libc::EPERM);
    let flags = libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
    for (domain, ty, proto, what) in [
        (libc::AF_INET, libc::SOCK_DGRAM, 0, "IPv4 UDP"),
        (
            libc::AF_INET,
            libc::SOCK_DGRAM | flags,
            0,
            "IPv4 UDP, flags",
        ),
        (libc::AF_INET6, libc::SOCK_DGRAM, 0, "IPv6 UDP"),
        (
            libc::AF_INET6,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            17,
            "IPv6 UDP, cloexec",
        ),
        (libc::AF_INET, libc::SOCK_RAW, 1, "IPv4 raw"),
        (libc::AF_INET, libc::SOCK_SEQPACKET, 0, "IPv4 seqpacket"),
        (
            libc::AF_INET,
            libc::SOCK_STREAM,
            libc::IPPROTO_MPTCP,
            "MPTCP",
        ),
        (
            libc::AF_INET6,
            libc::SOCK_STREAM | flags,
            132,
            "SCTP stream",
        ),
    ] {
        assert_eq!(
            socket_errno(domain, ty, proto),
            eperm,
            "{what} was not refused"
        );
    }
    // G2: the families are an ALLOW-LIST; seccomp answers before the kernel looks at the family, so
    // none of these loads a protocol module.
    for (domain, ty, what) in [
        (AF_SMC, libc::SOCK_STREAM, "AF_SMC (TCP past Landlock)"),
        (libc::AF_PACKET, libc::SOCK_RAW, "AF_PACKET"),
        (libc::AF_PACKET, libc::SOCK_DGRAM, "AF_PACKET datagram"),
        (AF_VSOCK, libc::SOCK_STREAM, "AF_VSOCK"),
        (AF_TIPC, libc::SOCK_STREAM, "AF_TIPC"),
        (AF_RDS, libc::SOCK_SEQPACKET, "AF_RDS"),
        (libc::AF_BLUETOOTH, libc::SOCK_STREAM, "AF_BLUETOOTH"),
        (libc::AF_ALG, libc::SOCK_SEQPACKET, "AF_ALG"),
        (0, libc::SOCK_STREAM, "AF_UNSPEC"),
    ] {
        assert_eq!(socket_errno(domain, ty, 0), eperm, "{what} was not refused");
    }
    let udp = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.raw_os_error());
    assert_eq!(
        udp.err(),
        Some(Some(libc::EPERM)),
        "std's UDP socket was made"
    );
    for (domain, ty, proto, what) in [
        (libc::AF_INET, libc::SOCK_STREAM, 0, "IPv4 TCP"),
        (
            libc::AF_INET6,
            libc::SOCK_STREAM | flags,
            libc::IPPROTO_TCP,
            "IPv6 TCP, flags",
        ),
        (libc::AF_UNIX, libc::SOCK_DGRAM, 0, "Unix datagram"),
        (libc::AF_UNIX, libc::SOCK_STREAM, 0, "Unix stream"),
        // glibc's resolver reads the routes through it.
        (libc::AF_NETLINK, libc::SOCK_RAW, 0, "netlink route"),
    ] {
        assert_eq!(socket_errno(domain, ty, proto), None, "{what} was refused");
    }

    // G7: TDLib never listens; an unbound `listen` would pick a TCP port by itself.
    for (domain, what) in [(libc::AF_INET, "TCP"), (libc::AF_UNIX, "Unix")] {
        // SAFETY: a socket made and closed here; `listen` on it takes no pointer.
        let r = unsafe {
            let fd = libc::socket(domain, libc::SOCK_STREAM, 0);
            assert!(fd >= 0, "no {what} socket to listen on");
            let r = libc::listen(fd, 1);
            let errno = io::Error::last_os_error().raw_os_error();
            libc::close(fd);
            (r, errno)
        };
        assert_eq!(r, (-1, eperm), "listen on a {what} socket was not refused");
    }

    // G4: a call of another ABI gets the filter's EPERM before any dispatch.
    // x32: the call number carries bit 30 (the kernel here may have x32 off, which alone would
    // answer ENOSYS: only the filter's check says EPERM).
    // SAFETY: socket's three integers; a socket that was made is closed.
    let x32 = unsafe {
        let r = libc::syscall(
            0x4000_0000 | libc::SYS_socket,
            libc::AF_INET,
            libc::SOCK_DGRAM,
            0,
        );
        if r >= 0 {
            libc::close(r as i32);
        }
        (r < 0).then(|| io::Error::last_os_error().raw_os_error())
    };
    assert_eq!(x32, Some(eperm), "an x32 system call was not refused");
    // An io_uring could make sockets past the filter. NULL parameters: EFAULT if it were allowed.
    // SAFETY: io_uring_setup with a NULL parameter block fails without touching memory.
    let r = unsafe { libc::syscall(libc::SYS_io_uring_setup, 1u32, std::ptr::null::<u8>()) };
    assert_eq!(
        (r, io::Error::last_os_error().raw_os_error()),
        (-1, eperm),
        "io_uring_setup was not refused"
    );

    metadata_checks(&root, &db);

    // Landlock: TCP only to the gate's port, no bind.
    TcpStream::connect(gate.local_addr().unwrap()).expect("the gate's port connects");
    if state == Landlock::On {
        // G7: from ABI 6 the domain is scoped: an abstract Unix socket made outside it, and a
        // signal to a process outside it (signal 0 is checked like any other), are out of reach.
        if lockdown::landlock_abi().is_some_and(|abi| abi >= 6) {
            let to_abstract =
                UnixStream::connect_addr(&abstract_addr).map_err(|e| e.raw_os_error());
            assert_eq!(
                to_abstract.err(),
                Some(Some(libc::EPERM)),
                "an abstract Unix socket outside the domain was reached"
            );
            // SAFETY: signal 0 only asks whether the parent could be signalled.
            let r = unsafe { libc::kill(libc::getppid(), 0) };
            assert_eq!(
                (r, io::Error::last_os_error().raw_os_error()),
                (-1, eperm),
                "a process outside the domain could be signalled"
            );
            println!("lockdown-child scoped=yes");
        } else {
            println!("lockdown-child scoped=no");
        }
        let to_other =
            TcpStream::connect(other.local_addr().unwrap()).map_err(|e| e.raw_os_error());
        assert_eq!(
            to_other.err(),
            Some(Some(libc::EACCES)),
            "another loopback port connected"
        );
        let bind = TcpListener::bind("127.0.0.1:0").map_err(|e| e.raw_os_error());
        assert_eq!(
            bind.err(),
            Some(Some(libc::EACCES)),
            "a TCP bind went through"
        );
    }
    drop(abstract_listener);
    if state == Landlock::On {
        filesystem_checks(&root, &db, &files, &system);
        // K8 (the security review's L2): the directory named as a system file got no rule.
        assert_eq!(
            errno(std::fs::read(sysdir.join("inner"))),
            Some(libc::EACCES),
            "a system file that is a directory made its whole tree readable"
        );
        println!("lockdown-child filesystem=checked");
    }
    println!("lockdown-child landlock={}", state.as_str());
}

fn errno<T>(r: io::Result<T>) -> Option<i32> {
    r.err().and_then(|e| e.raw_os_error())
}

fn c_path(path: &std::path::Path) -> std::ffi::CString {
    std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("a path without NUL")
}

/// The errno of a libc call that returned -1, `None` for any other return.
fn failed_with(r: libc::c_long) -> Option<i32> {
    (r == -1).then(|| io::Error::last_os_error().raw_os_error().unwrap_or(0))
}

/// K3 (the security review's M1): the path-based metadata calls are `EPERM` from the seccomp
/// filter whatever the file (here the child's own `root/victim`, which Landlock knows nothing
/// about: this part runs where Landlock is unavailable too), the descriptor-based ones work on a
/// file of `tdlib/` (SQLite uses `fchmod`; TDLib's `Stat.cpp` uses `futimens`).
fn metadata_checks(root: &std::path::Path, db: &std::path::Path) {
    use std::os::fd::AsRawFd as _;
    let eperm = Some(libc::EPERM);
    let victim = c_path(&root.join("victim"));
    // SAFETY (this block and the ones below): libc calls with NUL-terminated paths and values that
    // outlive the call; a call that is not refused only changes the child's own scratch file.
    unsafe {
        assert_eq!(
            failed_with(libc::chmod(victim.as_ptr(), 0o644).into()),
            eperm,
            "chmod(root/victim) was not refused"
        );
        assert_eq!(
            failed_with(libc::fchmodat(libc::AT_FDCWD, victim.as_ptr(), 0o644, 0).into()),
            eperm,
            "fchmodat(AT_FDCWD, root/victim) was not refused"
        );
        // fchmodat2 (kernel 6.6, flags other than 0 take it in newer glibc), raw.
        assert_eq!(
            failed_with(libc::syscall(
                libc::SYS_fchmodat2,
                libc::AT_FDCWD,
                victim.as_ptr(),
                0o644,
                0
            )),
            eperm,
            "fchmodat2(AT_FDCWD, root/victim) was not refused"
        );
        let (uid, gid) = (libc::geteuid(), libc::getegid());
        assert_eq!(
            failed_with(libc::chown(victim.as_ptr(), uid, gid).into()),
            eperm,
            "chown(root/victim) was not refused"
        );
        assert_eq!(
            failed_with(libc::lchown(victim.as_ptr(), uid, gid).into()),
            eperm,
            "lchown(root/victim) was not refused"
        );
        assert_eq!(
            failed_with(libc::fchownat(libc::AT_FDCWD, victim.as_ptr(), uid, gid, 0).into()),
            eperm,
            "fchownat(root/victim) was not refused"
        );
        let name = c"user.kourier";
        let value = b"x";
        assert_eq!(
            failed_with(
                libc::setxattr(victim.as_ptr(), name.as_ptr(), value.as_ptr().cast(), 1, 0).into()
            ),
            eperm,
            "setxattr(root/victim) was not refused"
        );
        assert_eq!(
            failed_with(
                libc::lsetxattr(victim.as_ptr(), name.as_ptr(), value.as_ptr().cast(), 1, 0).into()
            ),
            eperm,
            "lsetxattr(root/victim) was not refused"
        );
        assert_eq!(
            failed_with(libc::removexattr(victim.as_ptr(), name.as_ptr()).into()),
            eperm,
            "removexattr(root/victim) was not refused"
        );
        assert_eq!(
            failed_with(libc::lremovexattr(victim.as_ptr(), name.as_ptr()).into()),
            eperm,
            "lremovexattr(root/victim) was not refused"
        );
        // utimensat WITH a path.
        let times = [libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        }; 2];
        assert_eq!(
            failed_with(libc::utimensat(libc::AT_FDCWD, victim.as_ptr(), times.as_ptr(), 0).into()),
            eperm,
            "utimensat(AT_FDCWD, root/victim) was not refused"
        );
        // The legacy x86-64 calls, raw (glibc may route its wrappers through utimensat).
        #[cfg(target_arch = "x86_64")]
        {
            let utimbuf = [1 as libc::c_long, 1];
            assert_eq!(
                failed_with(libc::syscall(
                    libc::SYS_utime,
                    victim.as_ptr(),
                    utimbuf.as_ptr()
                )),
                eperm,
                "utime(root/victim) was not refused"
            );
            assert_eq!(
                failed_with(libc::syscall(
                    libc::SYS_utimes,
                    victim.as_ptr(),
                    std::ptr::null::<u8>()
                )),
                eperm,
                "utimes(root/victim) was not refused"
            );
            assert_eq!(
                failed_with(libc::syscall(
                    libc::SYS_futimesat,
                    libc::AT_FDCWD,
                    victim.as_ptr(),
                    std::ptr::null::<u8>()
                )),
                eperm,
                "futimesat(root/victim) was not refused"
            );
        }
    }
    // What stays: the descriptor-based calls on a file the child holds (a scratch file of its own
    // in tdlib/ under Landlock, or the plain directory where Landlock is unavailable).
    let meta = std::fs::File::create(db.join("meta")).expect("a file in tdlib/");
    let fd = meta.as_raw_fd();
    // SAFETY: libc calls on a descriptor of this process.
    unsafe {
        assert_eq!(
            libc::fchmod(fd, 0o600),
            0,
            "fchmod on a descriptor was refused"
        );
        assert_eq!(
            libc::fchown(fd, libc::geteuid(), libc::getegid()),
            0,
            "fchown on a descriptor was refused"
        );
        // glibc's futimens is utimensat(fd, NULL, times, 0).
        let times = [libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        }; 2];
        assert_eq!(
            libc::futimens(fd, times.as_ptr()),
            0,
            "futimens on a file of tdlib/ was refused"
        );
        // fsetxattr / fremovexattr reach the file system: not the filter's EPERM (the answer is
        // that file system's: success, or ENOTSUP where user attributes are off).
        let name = c"user.kourier";
        let r = libc::fsetxattr(fd, name.as_ptr(), b"x".as_ptr().cast(), 1, 0);
        assert_ne!(
            failed_with(r.into()),
            eperm,
            "fsetxattr was refused by the filter"
        );
        let r = libc::fremovexattr(fd, name.as_ptr());
        assert_ne!(
            failed_with(r.into()),
            eperm,
            "fremovexattr was refused by the filter"
        );
    }
    std::fs::remove_file(db.join("meta")).ok();
}

/// TG-2b's filesystem rules, seen from inside (pre3 brief §2.6's child table).
fn filesystem_checks(
    root: &std::path::Path,
    db: &std::path::Path,
    files: &std::path::Path,
    system: &std::path::Path,
) {
    use std::io::{Read as _, Write as _};
    let eacces = Some(libc::EACCES);
    // What the binlog and SQLite need in tdlib/: create, write, read, truncate, a same-directory
    // rename over an existing file, unlink, a directory made and removed.
    let bin = db.join("td.binlog");
    let mut f = std::fs::File::create(&bin).expect("create in tdlib/");
    f.write_all(b"binlog").expect("write in tdlib/");
    f.set_len(3).expect("truncate in tdlib/");
    drop(f);
    let mut text = String::new();
    std::fs::File::open(&bin)
        .and_then(|mut f| f.read_to_string(&mut text))
        .expect("read in tdlib/");
    assert_eq!(text, "bin");
    std::fs::write(db.join("td.binlog.new"), b"new").expect("a second file in tdlib/");
    std::fs::rename(db.join("td.binlog.new"), &bin).expect("rename inside tdlib/");
    std::fs::remove_file(&bin).expect("unlink in tdlib/");
    std::fs::create_dir(db.join("sub")).expect("mkdir in tdlib/");
    std::fs::remove_dir(db.join("sub")).expect("rmdir in tdlib/");
    // The files directory: a type directory made, a finished temporary file renamed into it
    // (REFER inside the one rule).
    std::fs::create_dir(files.join("photos")).expect("mkdir in the files directory");
    std::fs::rename(
        files.join("temp").join("part"),
        files.join("photos").join("p.jpg"),
    )
    .expect("rename from temp/ into photos/");
    // Outside every rule: no read, no create (the account directories' parent included).
    assert_eq!(
        errno(std::fs::read("/etc/passwd")),
        eacces,
        "/etc/passwd was read"
    );
    let exe = std::env::current_exe().expect("exe");
    assert_eq!(
        errno(std::fs::read_dir(exe.parent().expect("dir"))),
        eacces,
        "the test binary's directory was listed"
    );
    assert_eq!(
        errno(std::fs::write(root.join("new"), b"x")),
        eacces,
        "a file was made in the account directories' parent"
    );
    // The system file: read only.
    assert_eq!(
        std::fs::read(system).expect("read the system file"),
        b"system"
    );
    assert_eq!(
        errno(std::fs::OpenOptions::new().write(true).open(system)),
        eacces,
        "the read-only system file was opened for writing"
    );
    // No special node in tdlib/, nothing executed.
    assert_eq!(
        errno(std::os::unix::fs::symlink("/etc/passwd", db.join("link"))),
        eacces,
        "a symlink was made"
    );
    let fifo =
        std::ffi::CString::new(db.join("fifo").into_os_string().into_encoded_bytes()).unwrap();
    // SAFETY: mkfifo with a NUL-terminated path.
    let made = unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) };
    assert_eq!(
        (made, io::Error::last_os_error().raw_os_error()),
        (-1, eacces),
        "a FIFO was made"
    );
    assert_eq!(
        errno(UnixListener::bind(db.join("sock"))),
        eacces,
        "a Unix socket node was made"
    );
    // Nothing outside the rules is executed: not even this binary may be executed again (a
    // program, so the lane's process rule is kept: only a re-execution of itself). The EACCES
    // here comes from READ_FILE too (the binary lies outside every rule), so this line cannot tell
    // EXECUTE handled from unhandled (mutant A1d survives it, pre3 build 1): the unit test
    // `lockdown::tests::every_right_the_abi_knows_is_handled` pins EXECUTE in the handled set; an
    // execution from INSIDE `tdlib/` (where READ_FILE is allowed) would need a program start
    // other than the re-execution of this binary, which the lane's process rule forbids.
    assert_eq!(
        errno(
            std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--list")
                .status()
        ),
        eacces,
        "a program was executed"
    );
    // K6 (the security review's M4): the over-grants every positive row lets through. The rights
    // that change an EXISTING file or directory outside the rules (`root/victim`,
    // `root/emptydir`, made before the lock-down) and the account directories themselves.
    let victim = root.join("victim");
    let exdev_or_eacces = |r: Option<i32>| r == eacces || r == Some(libc::EXDEV);
    assert_eq!(
        errno(std::fs::OpenOptions::new().write(true).open(&victim)),
        eacces,
        "root/victim was opened for writing (no create)"
    );
    assert_eq!(
        errno(std::fs::OpenOptions::new().append(true).open(&victim)),
        eacces,
        "root/victim was opened for appending"
    );
    // SAFETY: truncate with a NUL-terminated path.
    let truncated = unsafe { libc::truncate(c_path(&victim).as_ptr(), 0) };
    assert_eq!(
        (truncated, io::Error::last_os_error().raw_os_error()),
        (-1, eacces),
        "root/victim was truncated"
    );
    assert_eq!(
        errno(std::fs::remove_file(&victim)),
        eacces,
        "root/victim was removed"
    );
    assert_eq!(
        errno(std::fs::create_dir(root.join("d2"))),
        eacces,
        "a directory was made beside the account directories"
    );
    assert_eq!(
        errno(std::fs::remove_dir(root.join("emptydir"))),
        eacces,
        "root/emptydir was removed"
    );
    assert!(
        exdev_or_eacces(errno(std::fs::rename(&victim, root.join("v2")))),
        "root/victim was renamed"
    );
    assert_eq!(
        errno(std::os::unix::fs::symlink("victim", root.join("s"))),
        eacces,
        "a symlink was made beside the account directories"
    );
    let outside_fifo = c_path(&root.join("f"));
    // SAFETY: mkfifo with a NUL-terminated path.
    let made = unsafe { libc::mkfifo(outside_fifo.as_ptr(), 0o600) };
    assert_eq!(
        (made, io::Error::last_os_error().raw_os_error()),
        (-1, eacces),
        "a FIFO was made beside the account directories"
    );
    // The account directories themselves cannot be removed or moved away (the check is on their
    // PARENT, which no rule names).
    assert_eq!(
        errno(std::fs::remove_dir(db)),
        eacces,
        "tdlib/ itself was removed"
    );
    assert!(
        exdev_or_eacces(errno(std::fs::rename(files, root.join("files2")))),
        "the files directory was moved away"
    );
    std::fs::write(db.join("f"), b"f").expect("a file in tdlib/");
    assert!(
        exdev_or_eacces(errno(std::fs::rename(db.join("f"), root.join("f")))),
        "a file of tdlib/ was moved out"
    );
    std::fs::remove_file(db.join("f")).expect("unlink in tdlib/");
    // No hard link from outside into the files directory (REFER is needed on the source side).
    assert!(
        exdev_or_eacces(errno(std::fs::hard_link(&victim, files.join("x")))),
        "root/victim was hard-linked into the files directory"
    );
    assert!(victim.exists(), "root/victim is gone");
    assert!(root.join("emptydir").is_dir(), "root/emptydir is gone");
    // The two trees do not leak into each other.
    std::fs::write(db.join("move"), b"m").expect("a file in tdlib/");
    let moved = errno(std::fs::rename(db.join("move"), files.join("move")));
    assert!(
        moved == Some(libc::EXDEV) || moved == eacces,
        "a file moved from tdlib/ into the files directory ({moved:?})"
    );
    // K2 (the security review's H2) documents the gap the files-directory option closes: a finished
    // download that TDLib keeps in the DATABASE directory (`db/temp/x` renamed into
    // `db/thumbnails/`, what `create_from_temp` does without `store_all_files_in_files_directory`)
    // is a rename across two directories of `tdlib/`, which has no REFER: EXDEV. The helper sets
    // the option, so every download lives in the files directory (which has REFER, checked above).
    std::fs::create_dir(db.join("temp")).expect("mkdir db/temp");
    std::fs::create_dir(db.join("thumbnails")).expect("mkdir db/thumbnails");
    std::fs::write(db.join("temp").join("x"), b"thumb").expect("a temporary file in db/temp");
    assert_eq!(
        errno(std::fs::rename(
            db.join("temp").join("x"),
            db.join("thumbnails").join("x")
        )),
        Some(libc::EXDEV),
        "a rename across two directories of tdlib/ did not fail EXDEV (it has no REFER)"
    );
}

#[test]
fn the_lock_down_leaves_tcp_to_the_gate_only_and_no_udp() {
    let out = run_child("lockdown::locked_down_child", CHILD_ENV, "child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("test result: ok. 1 passed"),
        "the locked-down child failed (its checks are in lockdown::locked_down_child): {}",
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .filter(|l| l.contains("panicked") || l.contains("was") || l.contains("left"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let state = stdout
        .lines()
        .find_map(|l| l.strip_prefix("lockdown-child landlock="))
        .expect("the child named its Landlock state");
    // What the child reached beyond the TCP rules (ABI 6 scopes), for the report.
    if let Some(scoped) = stdout
        .lines()
        .find_map(|l| l.strip_prefix("lockdown-child scoped="))
    {
        println!("lockdown scoped={scoped}");
    }
    match state {
        "on" => assert!(
            stdout.contains("lockdown-child filesystem=checked"),
            "the filesystem checks did not run: {stdout}"
        ),
        "unavailable" if std::env::var_os(REQUIRE_ENV).is_some_and(|v| v == "1") => {
            panic!("Landlock is unavailable here and {REQUIRE_ENV}=1")
        }
        "unavailable" => println!("SKIPPED the Landlock part: Landlock is unavailable here"),
        other => panic!("an unknown Landlock state {other:?}"),
    }
}

/// The i386 child (G4): locks itself down, then makes `socket(AF_INET, SOCK_DGRAM, 0)` through `int
/// 0x80` (i386's number 359). The filter answers EPERM before any dispatch (another audit arch);
/// without its arch check the call makes a UDP socket. A kernel without the 32-bit entry
/// (`ia32_emulation=off`) kills the caller with SIGSEGV instead: the parent says so.
#[cfg(target_arch = "x86_64")]
#[test]
fn locked_down_i386_child() {
    if std::env::var_os(I386_CHILD_ENV).is_none() {
        return;
    }
    let (_root, db, files) = account_dirs("i386");
    let dirs = FsPaths {
        database_dir: &db,
        files_dir: &files,
    };
    lockdown::apply_on_this_thread(40001, &dirs).expect("the lock-down applies");
    let ret: i32;
    // SAFETY: one raw 32-bit system call with integer arguments; `xchg` keeps rbx, which LLVM
    // reserves, out of the operands.
    unsafe {
        std::arch::asm!(
            "xchg rsi, rbx",
            "int 0x80",
            "xchg rsi, rbx",
            inout("rsi") i64::from(libc::AF_INET) => _,
            inlateout("eax") 359i32 => ret,
            in("ecx") libc::SOCK_DGRAM,
            in("edx") 0i32,
            clobber_abi("C"),
        );
    }
    // A negative value is -errno; a descriptor is >= 0.
    let answer = if ret < 0 { "refused" } else { "socket-made" };
    println!("lockdown-i386 {answer} errno={}", -ret.min(0));
}

#[cfg(target_arch = "x86_64")]
#[test]
fn an_i386_system_call_is_refused_by_the_lock_down() {
    use std::os::unix::process::ExitStatusExt as _;

    let out = run_child("lockdown::locked_down_i386_child", I386_CHILD_ENV, "i386");
    let stdout = String::from_utf8_lossy(&out.stdout);
    if out.status.signal() == Some(libc::SIGSEGV) {
        println!("SKIPPED the i386 part: this kernel has no 32-bit entry (int 0x80)");
        return;
    }
    assert!(
        out.status.success(),
        "the i386 child failed ({:?}): {stdout}",
        out.status
    );
    println!(
        "lockdown i386 child: {}",
        stdout
            .lines()
            .find(|l| l.starts_with("lockdown-i386"))
            .unwrap_or("no line")
    );
    assert!(
        stdout.contains(&format!("lockdown-i386 refused errno={}", libc::EPERM)),
        "an i386 UDP socket was not refused with EPERM: {stdout}"
    );
}

/// The helper's `apply` refuses a process with a second thread (a thread made before Landlock's
/// `restrict_self` keeps every right), and refuses it BEFORE it changes anything: this test
/// process (libtest runs the test on a thread of its own) stays without no_new_privs.
#[test]
fn a_process_with_two_threads_is_not_locked_down() {
    // A second thread for sure, whichever thread libtest runs this on.
    let (stop, wait) = std::sync::mpsc::channel::<()>();
    let other = std::thread::spawn(move || {
        let _ = wait.recv();
    });
    let nowhere = std::path::Path::new("/nonexistent/kourier-test");
    let dirs = FsPaths {
        database_dir: nowhere,
        files_dir: nowhere,
    };
    assert_eq!(lockdown::apply(40001, &dirs), Err(LockDownError::Threads));
    assert_eq!(
        status_field("NoNewPrivs:"),
        "0",
        "apply changed the process"
    );
    drop(stop);
    other.join().expect("the second thread ends");
}

/// TG-2b: a required account directory that is missing, not a directory, a symlink to one, not
/// this uid's or wider than 0700 fails the lock-down with `Paths` BEFORE anything is restricted
/// (this test process keeps no_new_privs off). Runs in the test process: nothing is applied.
#[test]
fn a_bad_account_directory_fails_the_lock_down_before_anything_is_restricted() {
    let (root, db, files) = account_dirs(&format!("bad-{}", std::process::id()));
    let wide = root.join("wide");
    std::fs::create_dir(&wide).unwrap();
    std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o755)).unwrap();
    let link = root.join("link");
    std::os::unix::fs::symlink(&db, &link).unwrap();
    let file = root.join("file");
    std::fs::write(&file, b"").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
    let missing = root.join("missing");
    let relative = std::path::Path::new("tdlib");
    // K7 (the security review's L1): the special mode bits count. The mask is 0o7777, so a setgid
    // or a sticky directory with 0700 permissions is not 0700.
    let setgid = root.join("setgid");
    let sticky = root.join("sticky");
    for (dir, mode) in [(&setgid, 0o2700), (&sticky, 0o1700)] {
        std::fs::create_dir(dir).unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
        let got = std::fs::metadata(dir).unwrap().permissions().mode() & 0o7777;
        assert_eq!(got, mode, "the kernel did not keep the special bit");
    }
    // A 0700 directory that is another uid's (`/root`, when it is one): the owner check.
    let mut bad_dirs = vec![
        missing,
        wide,
        link,
        file,
        relative.to_path_buf(),
        setgid,
        sticky,
    ];
    {
        use std::os::unix::fs::MetadataExt as _;
        // SAFETY: geteuid takes no argument and cannot fail.
        let me = unsafe { libc::geteuid() };
        match std::fs::metadata("/root") {
            Ok(m) if m.uid() != me && m.mode() & 0o7777 == 0o700 => {
                bad_dirs.push(PathBuf::from("/root"));
            }
            _ => println!("SKIPPED the other-owner row: /root is no 0700 directory of another uid"),
        }
    }
    for bad in &bad_dirs {
        for dirs in [
            FsPaths {
                database_dir: bad,
                files_dir: &files,
            },
            FsPaths {
                database_dir: &db,
                files_dir: bad,
            },
        ] {
            assert_eq!(
                lockdown::apply_on_this_thread(40001, &dirs),
                Err(LockDownError::Paths),
                "{dirs:?}"
            );
        }
    }
    assert_eq!(
        status_field("NoNewPrivs:"),
        "0",
        "a failed lock-down changed the process"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The newest Landlock ABI `lockdown::handled_fs` knows (the table's last row: nothing new in 5..=8).
const KNOWN_ABI: libc::c_long = 8;

/// What the kernel offers, for the report (a number, nothing else). K11: an ABI above the table's
/// last row may have added a filesystem right the rules do not handle (so allow); the line says to
/// re-check `handled_fs`. A WARNING, not a failure: a kernel update must not break ci.
#[test]
fn the_landlock_abi_is_reported() {
    match lockdown::landlock_abi() {
        Some(abi) => {
            println!("landlock abi {abi}");
            if abi > KNOWN_ABI {
                println!(
                    "Landlock ABI {abi} is above the table ({KNOWN_ABI}): re-check handled_fs"
                );
            }
        }
        None => println!("landlock unavailable"),
    }
}
