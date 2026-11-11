//! What protects a profile that holds a real account when the debug build runs on it
//! (`cargo xtask smoke`), with the real binary:
//!
//! - `KOURIER_DEV_NO_DUMP=1` makes the debug build non-dumpable like a release build: its
//!   memory cannot be read through `/proc`, and a process that is made to abort leaves no
//!   core dump (architecture §9.4). Without the variable a debug build stays dumpable.
//! - One process per profile: a second start on the same profile is refused by the lock on
//!   its data directory, also where no bus name could say so (offscreen, desktop services
//!   off), and it changes nothing.
//!
//! Debug builds only (the switch and the development keyring exist only there). Nothing here
//! links an account or opens a connection: the profiles are empty.

#![cfg(debug_assertions)]

use std::io::{BufRead as _, BufReader, Read as _};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(90);

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The binary offscreen, without desktop services, on `home` with the development keyring.
fn kourier(dir: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kourier"));
    cmd.arg("--dev-control")
        .env("KOURIER_HOME", home)
        .env("KOURIER_DEV_KEYRING", "passphrase")
        .env("KOURIER_DESKTOP", "0")
        .env_remove("KOURIER_SIGNAL_ENV")
        .env_remove("KOURIER_FAILPOINT")
        .env_remove("KOURIER_DEV_NO_DUMP")
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("QT_QUICK_BACKEND", "software")
        .env("QT_SCALE_FACTOR", "1")
        .env("QT_FORCE_STDERR_LOGGING", "1")
        .env("TMPDIR", dir)
        .env("LANG", "en_US.UTF-8")
        .env("LANGUAGE", "en")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    cmd
}

/// Starts `cmd` and waits until its development channel is up (keys are loaded by then).
fn started(mut cmd: Command, log: &Path) -> Child {
    let mut child = cmd
        .stderr(Stdio::from(std::fs::File::create(log).expect("log")))
        .spawn()
        .expect("start kourier");
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.contains("\"channel\"") {
                let _ = tx.send(());
            }
        }
    });
    rx.recv_timeout(WAIT)
        .unwrap_or_else(|_| panic!("the channel did not come up (log: {})", log.display()));
    child
}

fn wait_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        assert!(Instant::now() < deadline, "kourier did not exit");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether this user may read the process's memory map summary: only of a dumpable process.
fn memory_readable(pid: u32) -> bool {
    std::fs::read(format!("/proc/{pid}/smaps_rollup")).is_ok()
}

#[test]
fn a_debug_build_asked_not_to_dump_hides_its_memory_and_leaves_no_core() {
    let dir = scratch("hardening-dump");

    // The control: a debug build is dumpable by itself. (It is quit, not aborted: an abort
    // of a dumpable process is exactly the core dump this test is about.)
    let mut plain = started(kourier(&dir, &dir.join("plain")), &dir.join("plain.log"));
    assert!(
        memory_readable(plain.id()),
        "a debug build without the switch stays dumpable"
    );
    drop(plain.stdin.take());
    assert!(wait_exit(&mut plain).success());

    let mut cmd = kourier(&dir, &dir.join("guarded"));
    cmd.env("KOURIER_DEV_NO_DUMP", "1");
    let mut guarded = started(cmd, &dir.join("guarded.log"));
    let pid = guarded.id();
    assert!(
        !memory_readable(pid),
        "with the switch the process is not dumpable"
    );
    // Made to abort, as a failed debug assertion or a failpoint would: the kernel writes no
    // core for a process whose dumpable flag is off, whatever `core_pattern` pipes to.
    let killed = Command::new("kill")
        .args(["-ABRT", &pid.to_string()])
        .status()
        .expect("kill");
    assert!(killed.success());
    let status = wait_exit(&mut guarded);
    assert_eq!(status.signal(), Some(6), "ended by SIGABRT: {status:?}");
    assert!(
        !status.core_dumped(),
        "no core dump was written: {status:?}"
    );
}

#[test]
fn a_second_process_on_a_profile_is_refused_and_changes_nothing() {
    let dir = scratch("hardening-lock");
    let home = dir.join("home");
    let mut first = started(kourier(&dir, &home), &dir.join("first.log"));
    let keyring = home.join("kourier/config/dev-keyring.json");
    let before = std::fs::read(&keyring).expect("the first start made the keyring");

    // No session bus name exists here (desktop services off): only the lock can say no.
    let second = kourier(&dir, &home)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("second start");
    assert_eq!(second.status.code(), Some(1), "{second:?}");
    let said = String::from_utf8_lossy(&second.stderr);
    assert!(
        said.contains("another Kourier process is using this profile"),
        "{said}"
    );
    assert!(
        first.try_wait().expect("try_wait").is_none(),
        "the first runs on"
    );
    assert_eq!(std::fs::read(&keyring).expect("keyring"), before);

    drop(first.stdin.take());
    assert!(wait_exit(&mut first).success());
    // The lock went with its holder: the profile starts again.
    let mut again = started(kourier(&dir, &home), &dir.join("again.log"));
    drop(again.stdin.take());
    assert!(wait_exit(&mut again).success());
}

/// A list of `--print-networks` without Threema, which is on by default in every build (the
/// user's decision of 2026-11-09) and has its own test.
fn without_threema(kinds: Vec<&str>) -> Vec<&str> {
    kinds
        .into_iter()
        .filter(|kind| *kind != "Threema")
        .collect()
}

/// `kourier --print-networks` (hidden; `cargo xtask dist` runs it on the packed RELEASE binary): the
/// real debug binary says which networks a start of the profile would register and which it would
/// list as off, from `kourier.toml` of the profile: with WhatsApp switched on it registers it, else
/// it lists it as off; Telegram it names in neither list, switched on or not, because the binary
/// has no API pair (TG-2 critic H3: `Unavailable(ApiKey)` while on, `Absent` while off; none is
/// compiled in, and the test's profile is no Telegram run profile). It is run as the dist check runs it (an empty environment but the profile,
/// no display, no bus, no keyring service) and starts nothing: no window, and no data directory,
/// database or lock is made on the profile.
#[test]
fn print_networks_says_what_a_start_would_register_and_starts_nothing() {
    for (name, switched) in [
        ("off", None),
        ("on", Some("WhatsApp")),
        ("telegram", Some("Telegram")),
    ] {
        let on = switched == Some("WhatsApp");
        let text = switched.map(|kind| format!("networks-on = [\"{}\"]\n", kind.to_lowercase()));
        let dir = scratch(&format!("print-networks-{name}"));
        let home = dir.join("home");
        let prefs = home.join("kourier/config/kourier.toml");
        if let Some(text) = &text {
            std::fs::create_dir_all(prefs.parent().expect("config dir")).expect("create");
            std::fs::write(&prefs, text).expect("write prefs");
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_kourier"))
            .arg("--print-networks")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &dir)
            .env("KOURIER_HOME", &home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start kourier");
        let status = wait_exit(&mut child);
        let (mut out, mut err) = (String::new(), String::new());
        child
            .stdout
            .take()
            .expect("stdout")
            .read_to_string(&mut out)
            .expect("stdout");
        child
            .stderr
            .take()
            .expect("stderr")
            .read_to_string(&mut err)
            .expect("stderr");
        assert!(status.success(), "{name}: {status:?}: {err}");
        let line = out.strip_suffix('\n').expect("a line");
        assert!(!line.contains('\n'), "{name}: one line: {out:?}");
        let list = |key: &str| -> Vec<&str> {
            line.split(' ')
                .find_map(|w| w.strip_prefix(&format!("{key}=")))
                .unwrap_or_else(|| panic!("{name}: no {key}= in {line:?}"))
                .split(',')
                .filter(|k| !k.is_empty())
                .collect()
        };
        assert!(line.starts_with("networks registered="), "{name}: {line:?}");
        // (A build without the WhatsApp backend has nothing to register or list; the switch still shows.)
        let built = cfg!(feature = "whatsapp");
        assert_eq!(
            list("registered").contains(&"WhatsApp"),
            built && on,
            "{name}: {line:?}"
        );
        assert_eq!(
            list("off").contains(&"WhatsApp"),
            built && !on,
            "{name}: {line:?}"
        );
        // The Telegram backend: this binary cannot log in (no API pair), so it is neither
        // registered nor listed as off, whatever the switch says (critic H3).
        assert!(
            !list("registered").contains(&"Telegram"),
            "{name}: {line:?}"
        );
        assert!(!list("off").contains(&"Telegram"), "{name}: {line:?}");
        // Threema is on by default (the user's decision of 2026-11-09) and has its own test.
        assert_eq!(
            without_threema(list("on")),
            switched.into_iter().collect::<Vec<_>>(),
            "{name}"
        );
        assert_eq!(
            list("registered").contains(&"Signal"),
            cfg!(feature = "signal"),
            "{name}: {line:?}"
        );
        // Nothing was started on the profile: the only thing below it is the file written above.
        if let Some(text) = &text {
            let mut files = Vec::new();
            let mut walk = vec![home.clone()];
            while let Some(d) = walk.pop() {
                for e in std::fs::read_dir(&d).expect("read_dir") {
                    let path = e.expect("entry").path();
                    if path.is_dir() {
                        walk.push(path);
                    } else {
                        files.push(path);
                    }
                }
            }
            assert_eq!(files, std::slice::from_ref(&prefs), "{name}: {files:?}");
            assert_eq!(
                &std::fs::read_to_string(&prefs).expect("prefs"),
                text,
                "{name}: the preferences were not touched"
            );
        } else {
            assert!(!home.exists(), "{name}: the profile was made");
        }
    }
}

/// TR R0a A4, the run tool's probe: the real debug binary with `KOURIER_DEV_REHEARSAL=telegram` and
/// `networks-on = ["telegram"]` says Telegram is REGISTERED (the scripted rehearsal fake holds the slot)
/// although the binary has no API pair, no helper files and no run profile; without the switch the same
/// profile registers it nowhere (the binary cannot log in); a value that is not a network fails the run
/// without echoing it. It starts nothing: nothing below the profile but the preferences written here
/// (no `tg-run` directory, no data directory, no file of Telegram's).
#[cfg(all(feature = "telegram", feature = "fake"))]
#[test]
fn print_networks_registers_the_telegram_rehearsal_without_an_api_pair_and_starts_nothing() {
    for (name, rehearsal) in [
        ("rehearsal", Some("telegram")),
        ("plain", None),
        ("typo", Some("Telegramm")),
    ] {
        let dir = scratch(&format!("print-networks-tg-rehearsal-{name}"));
        let home = dir.join("home");
        let prefs = home.join("kourier/config/kourier.toml");
        std::fs::create_dir_all(prefs.parent().expect("config dir")).expect("create");
        std::fs::write(&prefs, "networks-on = [\"telegram\"]\n").expect("write prefs");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kourier"));
        cmd.arg("--print-networks")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &dir)
            .env("KOURIER_HOME", &home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(value) = rehearsal {
            cmd.env("KOURIER_DEV_REHEARSAL", value);
        }
        let mut child = cmd.spawn().expect("start kourier");
        let status = wait_exit(&mut child);
        let (mut out, mut err) = (String::new(), String::new());
        child
            .stdout
            .take()
            .expect("stdout")
            .read_to_string(&mut out)
            .expect("stdout");
        child
            .stderr
            .take()
            .expect("stderr")
            .read_to_string(&mut err)
            .expect("stderr");
        if name == "typo" {
            assert!(!status.success(), "{name}: the run went on: {out:?}");
            assert!(
                !out.contains("Telegramm") && !err.contains("Telegramm"),
                "{name}: the value is repeated: {out:?} {err:?}"
            );
        } else {
            assert!(status.success(), "{name}: {status:?}: {err}");
            let line = out.strip_suffix('\n').expect("a line");
            let list = |key: &str| -> Vec<&str> {
                line.split(' ')
                    .find_map(|w| w.strip_prefix(&format!("{key}=")))
                    .unwrap_or_else(|| panic!("{name}: no {key}= in {line:?}"))
                    .split(',')
                    .filter(|k| !k.is_empty())
                    .collect()
            };
            assert_eq!(
                list("registered").contains(&"Telegram"),
                rehearsal.is_some(),
                "{name}: {line:?}"
            );
            assert!(!list("off").contains(&"Telegram"), "{name}: {line:?}");
            assert_eq!(
                without_threema(list("on")),
                ["Telegram"],
                "{name}: {line:?}"
            );
        }
        // Nothing was started on the profile: the only thing below it is the file written above.
        let mut files = Vec::new();
        let mut walk = vec![home.clone()];
        while let Some(d) = walk.pop() {
            for e in std::fs::read_dir(&d).expect("read_dir") {
                let path = e.expect("entry").path();
                if path.is_dir() {
                    walk.push(path);
                } else {
                    files.push(path);
                }
            }
        }
        assert_eq!(files, std::slice::from_ref(&prefs), "{name}: {files:?}");
    }
}

/// The Threema twin of `print_networks_says_what_a_start_would_register_and_starts_nothing`, after
/// the user's decision of 2026-11-09 ("enable Threema by default globally", "Any ID, every build").
/// A NORMAL start registers Threema with nothing in the file, also in a prepared run profile (the
/// test build's profile on another computer lives there: its list is not read), and lists it as
/// off once the user turned it off (`networks-off`). A start by the RUN TOOL
/// (`KOURIER_THREEMA_RUN`) keeps TH1 / lane decision Q2 and TH4 decision E1: registered only while
/// `networks-on` names it, and while it is off listed nowhere in a plain profile and as off only in
/// a prepared run profile (a kept profile with a list of test IDs, made the way the binary looks
/// for it: `HOME`, no `XDG_DATA_HOME`). Run as the dist check runs it, with a deadline; its stderr
/// is part of a failure; nothing is made on the profile except the files written here, whatever
/// the directory. Mutants: the defaults dropped from `start_prefs` -> `default`; `networks-off`
/// not read -> `turned-off`; the run tool not told apart -> `run-tool-off`; the marker
/// (`threema-run/` in the profile) not judged -> `marked-profile*`.
#[test]
fn print_networks_registers_threema_only_when_switched_on_and_starts_nothing() {
    const OFF: &str = "networks-off = [\"threema\"]\n";
    const ON: &str = "networks-on = [\"threema\"]\n";
    // (name, run tool, preferences, prepared, run tool's directory in the profile, registered,
    // listed as off)
    let cases = [
        ("default", false, None, false, false, true, false),
        ("turned-off", false, Some(OFF), false, false, false, true),
        ("prepared-normal", false, None, true, false, true, false),
        ("run-tool-off", true, None, false, false, false, false),
        ("run-tool-on", true, Some(ON), false, false, true, false),
        ("run-tool-prepared", true, None, true, false, false, true),
        // A profile that holds the run tool's `threema-run/` outside the tool: Threema neither
        // registered nor listed, whatever `networks-on` says (the tool's own start keeps its table).
        ("marked-profile", false, None, false, true, false, false),
        (
            "marked-profile-on",
            false,
            Some(ON),
            false,
            true,
            false,
            false,
        ),
        (
            "marked-profile-off",
            false,
            Some(OFF),
            false,
            true,
            false,
            false,
        ),
        (
            "marked-run-tool-on",
            true,
            Some(ON),
            false,
            true,
            true,
            false,
        ),
    ];
    for (name, run_tool, text, prepared, marked, registered, off) in cases {
        // A kept run profile must be canonical: the scratch directory may sit below a link.
        let dir = std::fs::canonicalize(scratch(&format!("print-networks-threema-{name}")))
            .expect("canonical scratch dir");
        let keeper = dir.join(".local/share/kourier-threema-run");
        let home = if prepared {
            keeper.join("p")
        } else {
            dir.join("home")
        };
        let prefs = home.join("kourier/config/kourier.toml");
        let ids_file = keeper.join("threema-allowed-ids");
        // `printf 'kourier-threema-allow-v1:TESTID01' | sha256sum`: a synthetic ID.
        let ids_text = "sha256:871ad9802d67bec50e0d5a9d2c4113efec962437723daac9d86b8f1e0ab82a87\n";
        if prepared {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::create_dir_all(&home).expect("create the run profile");
            std::fs::set_permissions(&keeper, std::fs::Permissions::from_mode(0o700))
                .expect("chmod the keeper root");
            std::fs::write(&ids_file, ids_text).expect("write the list");
            std::fs::set_permissions(&ids_file, std::fs::Permissions::from_mode(0o600))
                .expect("chmod the list");
        }
        if marked {
            std::fs::create_dir_all(home.join("threema-run")).expect("the run tool's directory");
        }
        if let Some(text) = text {
            std::fs::create_dir_all(prefs.parent().expect("config dir")).expect("create");
            std::fs::write(&prefs, text).expect("write prefs");
        }
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kourier"));
        cmd.arg("--print-networks")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &dir)
            .env("KOURIER_HOME", &home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if run_tool {
            cmd.env("KOURIER_THREEMA_RUN", "1");
        }
        let mut child = cmd.spawn().expect("start kourier");
        let status = wait_exit(&mut child);
        let (mut out, mut err) = (String::new(), String::new());
        child
            .stdout
            .take()
            .expect("stdout")
            .read_to_string(&mut out)
            .expect("stdout");
        child
            .stderr
            .take()
            .expect("stderr")
            .read_to_string(&mut err)
            .expect("stderr");
        assert!(status.success(), "{name}: {status:?}: {err}");
        let line = out.strip_suffix('\n').expect("a line");
        assert!(!line.contains('\n'), "{name}: one line: {out:?}");
        let list = |key: &str| -> Vec<&str> {
            line.split(' ')
                .find_map(|w| w.strip_prefix(&format!("{key}=")))
                .unwrap_or_else(|| panic!("{name}: no {key}= in {line:?}"))
                .split(',')
                .filter(|k| !k.is_empty())
                .collect()
        };
        assert!(line.starts_with("networks registered="), "{name}: {line:?}");
        let built = cfg!(feature = "threema");
        assert_eq!(
            list("registered").contains(&"Threema"),
            built && registered,
            "{name}: {line:?}"
        );
        assert_eq!(
            list("off").contains(&"Threema"),
            built && off,
            "{name}: {line:?}"
        );
        // The switch as this start reads it: on by default outside the run tool, `networks-on`
        // alone for the run tool.
        let on = if run_tool || marked {
            text == Some(ON)
        } else {
            text != Some(OFF)
        };
        assert_eq!(
            list("on"),
            if on { vec!["Threema"] } else { vec![] },
            "{name}: {line:?}"
        );
        assert_eq!(
            list("registered").contains(&"Signal"),
            cfg!(feature = "signal"),
            "{name}: {line:?}"
        );
        // Nothing was started on the profile: below it only the preferences written here, the list
        // untouched and still the only file of the keeper root beside the profile.
        let mut files = Vec::new();
        if home.exists() {
            let mut walk = vec![home.clone()];
            while let Some(d) = walk.pop() {
                for e in std::fs::read_dir(&d).expect("read_dir") {
                    let path = e.expect("entry").path();
                    if path.is_dir() {
                        walk.push(path);
                    } else {
                        files.push(path);
                    }
                }
            }
        } else {
            assert!(!prepared, "{name}: the run profile went");
        }
        let expected: Vec<_> = text.iter().map(|_| prefs.clone()).collect();
        assert_eq!(files, expected, "{name}: {files:?}");
        if let Some(text) = text {
            assert_eq!(
                std::fs::read_to_string(&prefs).expect("prefs"),
                text,
                "{name}: the preferences were not touched"
            );
        } else if !prepared && !marked {
            assert!(!home.exists(), "{name}: the profile was made");
        }
        if prepared {
            assert_eq!(
                std::fs::read_to_string(&ids_file).expect("list"),
                ids_text,
                "{name}: the list was touched"
            );
            let mut kept: Vec<_> = std::fs::read_dir(&keeper)
                .expect("read_dir")
                .map(|e| e.expect("entry").file_name())
                .collect();
            kept.sort();
            assert_eq!(kept, ["p", "threema-allowed-ids"], "{name}: {kept:?}");
        }
    }
}
