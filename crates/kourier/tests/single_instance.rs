//! Single instance with two processes (§8.2 step 1, §8.8), on a private `dbus-daemon` (never
//! the user's session bus): the first `kourier` owns the profile's bus name; a second launch of
//! the same profile hands its activation (with `$XDG_ACTIVATION_TOKEN`) or its `sgnl://` link
//! to the first one and exits 0 without opening anything; the first one shows its window. A
//! `--background` launch (autostart) exits 0 and asks nothing. A launch of another profile is
//! not affected.
//!
//! Debug builds only: the profile uses the development keyring (`KOURIER_DEV_KEYRING`) under a
//! scratch `KOURIER_HOME`, and the first instance is driven through `--dev-control`.

#![cfg(debug_assertions)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(90);

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A `dbus-daemon` with no service directories (nothing can be auto-activated on it).
struct PrivateBus {
    child: Child,
    address: String,
}

impl PrivateBus {
    fn start(dir: &Path) -> Self {
        let config = dir.join("bus.conf");
        std::fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                dir.display()
            ),
        )
        .expect("bus config");
        let mut child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--nopidfile", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("bus address");
        let address = line.trim().to_owned();
        assert!(!address.is_empty(), "dbus-daemon printed no address");
        Self { child, address }
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn kourier(dir: &Path, bus: &PrivateBus, home: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kourier"));
    cmd.env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .env_remove("DBUS_SYSTEM_BUS_ADDRESS")
        .env_remove("XDG_ACTIVATION_TOKEN")
        .env("KOURIER_HOME", dir.join(home))
        .env("KOURIER_DEV_KEYRING", "passphrase")
        // Desktop services on, although offscreen: the bus is private.
        .env("KOURIER_DESKTOP", "1")
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("QT_QUICK_BACKEND", "software")
        .env("QT_SCALE_FACTOR", "1")
        .env("QT_FORCE_STDERR_LOGGING", "1")
        .env("TMPDIR", dir)
        .env("LANG", "en_US.UTF-8")
        .env("LANGUAGE", "en");
    cmd
}

struct First {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
    log: PathBuf,
}

impl First {
    fn start(dir: &Path, bus: &PrivateBus) -> Self {
        let log = dir.join("first.log");
        let mut child = kourier(dir, bus, "home")
            .args(["--dev-control", "--size=900x700"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(std::fs::File::create(&log).expect("log")))
            .spawn()
            .expect("start the first kourier");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            lines,
            log,
        }
    }

    fn request(&mut self, id: u64, cmd: &str, args: Value) -> Value {
        let stdin = self.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{}", json!({"id": id, "cmd": cmd, "args": args})).expect("write");
        stdin.flush().expect("flush");
        let deadline = Instant::now() + WAIT;
        loop {
            match self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(v) if v["type"] == "response" && v["id"] == id => return v,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for {cmd}"),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("kourier exited before {cmd}: {}", self.log_text())
                }
            }
        }
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Waits until the log has `count` lines containing `needle`.
    fn wait_log(&self, needle: &str, count: usize) {
        let deadline = Instant::now() + WAIT;
        while self.log_text().matches(needle).count() < count {
            assert!(
                Instant::now() < deadline,
                "the first instance never logged {needle:?} {count} times:\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Runs a second launch to completion; returns its status and stderr.
fn second(
    dir: &Path,
    bus: &PrivateBus,
    home: &str,
    args: &[&str],
    token: Option<&str>,
) -> (ExitStatus, String) {
    let mut cmd = kourier(dir, bus, home);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(t) = token {
        cmd.env("XDG_ACTIVATION_TOKEN", t);
    }
    let started = Instant::now();
    let out = cmd.output().expect("second kourier");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the second launch took {:?}",
        started.elapsed()
    );
    (
        out.status,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_second_launch_activates_the_first_and_exits() {
    let dir = scratch("single-instance");
    let bus = PrivateBus::start(&dir);
    let mut first = First::start(&dir, &bus);
    let ready = first.request(1, "wait", json!({"until": "loaded", "timeout_ms": 60000}));
    assert_eq!(ready["ok"], true, "{ready}");

    // Activation with a token.
    let (status, stderr) = second(&dir, &bus, "home", &[], Some("token-from-launcher"));
    assert!(status.success(), "second launch failed: {status} {stderr}");
    assert!(stderr.contains("already running"), "{stderr}");
    first.wait_log("activation request from another launch", 1);
    let log = first.log_text();
    assert!(
        !log.contains("token-from-launcher"),
        "tokens are never logged"
    );

    // A sgnl:// link (M1: only shows the window, §9.4).
    let (status, stderr) = second(&dir, &bus, "home", &["sgnl://signal.me/#p/+15550100"], None);
    assert!(status.success(), "second launch failed: {status} {stderr}");
    first.wait_log("sgnl:// link: showing the window", 1);
    assert!(
        !first.log_text().contains("15550100"),
        "links are never logged"
    );

    // A background launch (autostart) leaves the running instance as it is.
    let requests = first
        .log_text()
        .matches("activation request from another launch")
        .count();
    let (status, stderr) = second(&dir, &bus, "home", &["--background"], Some("token-2"));
    assert!(
        status.success(),
        "background launch failed: {status} {stderr}"
    );
    assert!(stderr.contains("already running"), "{stderr}");

    // Still one window, still answering.
    let state = first.request(2, "state", Value::Null);
    assert_eq!(state["ok"], true, "{state}");
    assert_eq!(
        first
            .log_text()
            .matches("activation request from another launch")
            .count(),
        requests,
        "a background launch asks nothing"
    );

    // Another profile is another instance: it starts (and is quit by closing its stdin).
    let mut other = kourier(&dir, &bus, "other")
        .arg("--dev-control")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(
            std::fs::File::create(dir.join("other.log")).expect("log"),
        ))
        .spawn()
        .expect("other profile");
    let stdout = other.stdout.take().expect("stdout");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("other profile's first line");
    assert!(
        line.contains("\"channel\""),
        "another profile runs on its own: {line}"
    );
    drop(other.stdin.take());
    let status = other.wait().expect("other exits");
    assert!(status.success(), "{status}");

    let quit = first.request(3, "quit", Value::Null);
    assert_eq!(quit["ok"], true);
    drop(first.stdin.take());
    let status = first.child.wait().expect("first exits");
    assert!(status.success(), "{status}");

    // With the first instance gone, a launch runs on its own again.
    let mut again = kourier(&dir, &bus, "home")
        .arg("--dev-control")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("relaunch");
    let mut line = String::new();
    BufReader::new(again.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("relaunch's first line");
    assert!(line.contains("\"channel\""), "{line}");
    drop(again.stdin.take());
    assert!(again.wait().expect("exit").success());
}

/// S15: the browser hands a solved challenge back as a `signalcaptcha://` link, which the
/// desktop gives to a new launch of Kourier (the scheme handler of the desktop file). That
/// launch passes it to the running instance, which answers the challenge of the account that
/// waits for one; the second launch exits. The link is a secret: it is in no log.
/// (On the fake network's demo account: not in a build without the test network.)
#[cfg(feature = "fake")]
#[test]
fn a_signalcaptcha_link_from_a_second_launch_answers_the_challenge() {
    let dir = scratch("single-instance-captcha");
    let bus = PrivateBus::start(&dir);
    let log = dir.join("first.log");
    // The fake network's demo account ends in the challenge state once it is linked.
    let mut child = kourier(&dir, &bus, "home")
        .env("KOURIER_DEV_DEMO_STATE", "challenge")
        .args(["--dev-control", "--size=900x700"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(std::fs::File::create(&log).expect("log")))
        .spawn()
        .expect("start the first kourier");
    let stdout = child.stdout.take().expect("stdout");
    let (tx, lines) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(&line)
                && tx.send(v).is_err()
            {
                break;
            }
        }
    });
    let stdin = child.stdin.take();
    let mut first = First {
        child,
        stdin,
        lines,
        log,
    };
    let ok = |first: &mut First, id: u64, cmd: &str, args: Value| {
        let r = first.request(id, cmd, args);
        assert_eq!(r["ok"], true, "{cmd}: {r}\n{}", first.log_text());
        r["result"].clone()
    };
    ok(
        &mut first,
        1,
        "wait",
        json!({"until": "loaded", "timeout_ms": 60000}),
    );
    // Link the demo account: the device name, then its phone scans the last QR code.
    ok(&mut first, 2, "add_account", json!({"network": "Fake"}));
    ok(&mut first, 3, "wait", json!({"until": "login:input"}));
    ok(
        &mut first,
        4,
        "submit_login",
        json!({"values": ["Captcha test"]}),
    );
    // Its data arrives and the network asks for a challenge.
    ok(
        &mut first,
        5,
        "wait",
        json!({"until": "challenge", "timeout_ms": 120000}),
    );
    let state = ok(&mut first, 6, "state", Value::Null);
    assert!(state["challenge"].is_i64(), "{state}");
    assert_eq!(state["banner"], "challenge", "{state}");

    // A link that is no answer changes nothing.
    let (status, stderr) = second(&dir, &bus, "home", &["sgnl://signal.me/#p/+15550100"], None);
    assert!(status.success(), "second launch failed: {status} {stderr}");
    first.wait_log("sgnl:// link: showing the window", 1);
    let state = ok(&mut first, 7, "state", Value::Null);
    assert!(state["challenge"].is_i64(), "still waiting: {state}");

    // What the browser hands back after the challenge was solved there. A captcha token is
    // about 2,600 bytes today and has no documented maximum: a link more than twice as long
    // must pass the command line, the bus and the running instance whole.
    let answer = format!(
        "signalcaptcha://signal-hcaptcha.5fad97ac-7d06.challenge.secret-solution{}",
        "A".repeat(6000)
    );
    let (status, stderr) = second(
        &dir,
        &bus,
        "home",
        &[answer.as_str()],
        Some("token-from-browser"),
    );
    assert!(status.success(), "second launch failed: {status} {stderr}");
    assert!(stderr.contains("already running"), "{stderr}");
    first.wait_log("a solved challenge came back from the browser", 1);
    let waited = ok(
        &mut first,
        8,
        "wait",
        json!({"until": "no_challenge", "timeout_ms": 60000}),
    );
    assert_eq!(waited["observed"], Value::Null, "{waited}");
    let state = ok(&mut first, 9, "state", Value::Null);
    assert_eq!(state["challenge"], Value::Null, "{state}");
    assert_ne!(state["banner"], "challenge", "{state}");
    assert!(
        state["accounts"]
            .as_array()
            .is_some_and(|a| a.iter().any(|s| s == "online")),
        "the account is back online: {state}"
    );
    let log = first.log_text();
    assert!(
        !log.contains("secret-solution") && !log.contains("token-from-browser"),
        "answers and tokens are never logged"
    );

    let quit = first.request(10, "quit", Value::Null);
    assert_eq!(quit["ok"], true);
    drop(first.stdin.take());
    let status = first.child.wait().expect("first exits");
    assert!(status.success(), "{status}");
}

/// The desktop file makes Kourier the handler of `sgnl:` links and of `signalcaptcha:` links
/// (the challenge hand-off from the browser), takes them as arguments, and is valid.
#[test]
fn the_desktop_file_registers_the_link_schemes() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/app.kourier.Kourier.desktop");
    let text = std::fs::read_to_string(&path).expect("the desktop file");
    let value = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key).and_then(|rest| rest.strip_prefix('=')))
            .unwrap_or_else(|| panic!("no {key} in the desktop file"))
    };
    let types: Vec<&str> = value("MimeType")
        .split(';')
        .filter(|t| !t.is_empty())
        .collect();
    assert!(types.contains(&"x-scheme-handler/sgnl"), "{types:?}");
    assert!(
        types.contains(&"x-scheme-handler/signalcaptcha"),
        "{types:?}"
    );
    assert!(
        value("MimeType").ends_with(';'),
        "a list ends with a semicolon"
    );
    // The links arrive as arguments (%U), and a second launch hands them to the first.
    assert_eq!(value("Exec"), "kourier %U");
    assert_eq!(value("SingleMainWindow"), "true");
    // The validator of desktop-file-utils, where it is installed.
    match Command::new("desktop-file-validate").arg(&path).output() {
        Ok(out) => assert!(
            out.status.success(),
            "desktop-file-validate: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("desktop-file-validate is not installed: skipped");
        }
        Err(e) => panic!("desktop-file-validate: {e}"),
    }
}
