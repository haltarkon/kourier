//! The real phone: adb command lines over an [`Exec`].
//!
//! `Exec` runs one `adb <args>` and returns exit code and stdout bytes; [`ProcessExec`] really
//! spawns `adb`, tests use a recording one, so the argument vectors are asserted without adb.
//! Nothing here logs a command's output; errors name the adb subcommand and the exit code only.

use super::Phone;
use super::tree::{Node, UiTree};
use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub struct ExecOut {
    pub code: i32,
    pub stdout: Vec<u8>,
}

pub trait Exec {
    /// Run `adb <args>`; `Err` only when adb could not run at all (or timed out).
    fn run(&mut self, args: &[String]) -> Result<ExecOut>;
    fn now(&self) -> Duration;
    fn sleep(&mut self, d: Duration);
}

/// Spawns the `adb` program with a deadline per call.
pub struct ProcessExec {
    program: String,
    started: Instant,
    pub call_timeout: Duration,
}

impl ProcessExec {
    pub fn new() -> ProcessExec {
        ProcessExec {
            program: "adb".into(),
            started: Instant::now(),
            call_timeout: Duration::from_secs(60),
        }
    }
}

impl Default for ProcessExec {
    fn default() -> Self {
        Self::new()
    }
}

impl Exec for ProcessExec {
    fn run(&mut self, args: &[String]) -> Result<ExecOut> {
        let mut child = Command::new(&self.program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("cannot run adb")?;
        let mut out = child.stdout.take().context("no adb stdout")?;
        let reader = std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = out.read_to_end(&mut v);
            v
        });
        let deadline = Instant::now() + self.call_timeout;
        let status = loop {
            if let Some(s) = child.try_wait().context("waiting for adb")? {
                break s;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                bail!(
                    "adb {} timed out",
                    args.iter().find(|a| !a.starts_with('-')).map_or("", |s| s)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let stdout = reader.join().unwrap_or_default();
        Ok(ExecOut {
            code: status.code().unwrap_or(-1),
            stdout,
        })
    }
    fn now(&self) -> Duration {
        self.started.elapsed()
    }
    fn sleep(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}

static DUMP_SEQ: AtomicU64 = AtomicU64::new(0);

pub struct AdbPhone<E: Exec = ProcessExec> {
    serial: String,
    exec: E,
}

impl AdbPhone<ProcessExec> {
    pub fn new(serial: &str) -> Result<Self> {
        AdbPhone::with_exec(serial, ProcessExec::new())
    }
}

/// A token that goes into `adb shell ...` (adb joins shell arguments into one line for the device's
/// shell, so only a plain alphabet is allowed).
fn plain(what: &str, v: &str, extra: &str) -> Result<()> {
    if v.is_empty()
        || !v
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || extra.contains(c))
    {
        bail!("{what} has characters that are not allowed on a shell line");
    }
    Ok(())
}

impl<E: Exec> AdbPhone<E> {
    pub fn with_exec(serial: &str, exec: E) -> Result<Self> {
        if serial.is_empty() || serial.starts_with('-') || serial.chars().any(|c| c.is_whitespace())
        {
            bail!("not a usable adb serial");
        }
        Ok(AdbPhone {
            serial: serial.to_string(),
            exec,
        })
    }

    pub fn exec(&self) -> &E {
        &self.exec
    }

    fn argv(&self, tail: &[&str]) -> Vec<String> {
        let mut v = vec!["-s".to_string(), self.serial.clone()];
        v.extend(tail.iter().map(|s| s.to_string()));
        v
    }

    fn run_ok(&mut self, tail: &[&str]) -> Result<ExecOut> {
        let args = self.argv(tail);
        let out = self.exec.run(&args)?;
        if out.code != 0 {
            bail!(
                "adb {} failed (exit {})",
                tail.first().copied().unwrap_or(""),
                out.code
            );
        }
        Ok(out)
    }

    fn shell(&mut self, tail: &[&str]) -> Result<ExecOut> {
        let mut t = vec!["shell"];
        t.extend_from_slice(tail);
        self.run_ok(&t)
    }
}

fn center(node: &Node) -> Result<(i32, i32)> {
    let b = node.bounds();
    if b.is_empty() {
        bail!("the node has empty bounds, nothing to tap");
    }
    Ok(b.center())
}

impl<E: Exec> Phone for AdbPhone<E> {
    fn connect(&mut self) -> Result<()> {
        if self.serial.contains(':') {
            let out = self
                .exec
                .run(&["connect".to_string(), self.serial.clone()])?;
            let said = String::from_utf8_lossy(&out.stdout);
            if out.code != 0
                || !(said.contains("connected to") || said.contains("already connected"))
            {
                bail!("adb connect did not connect (exit {})", out.code);
            }
        }
        let out = self.run_ok(&["get-state"])?;
        if String::from_utf8_lossy(&out.stdout).trim() != "device" {
            bail!("the test phone is not in the `device` state");
        }
        Ok(())
    }

    fn wake(&mut self) -> Result<()> {
        self.shell(&["input", "keyevent", "KEYCODE_WAKEUP"])?;
        self.shell(&["wm", "dismiss-keyguard"])?;
        Ok(())
    }

    fn dump(&mut self) -> Result<UiTree> {
        let remote = format!(
            "/sdcard/kourier-ui-{}-{}.xml",
            std::process::id(),
            DUMP_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let res = (|| -> Result<UiTree> {
            let d = self.shell(&["uiautomator", "dump", &remote])?;
            if String::from_utf8_lossy(&d.stdout).contains("ERROR") {
                bail!("uiautomator could not dump the screen");
            }
            let raw = self.run_ok(&["exec-out", "cat", &remote])?;
            let xml = String::from_utf8(raw.stdout)
                .map_err(|_| anyhow::anyhow!("the dump is not UTF-8"))?;
            Ok(UiTree::parse(&xml)?)
        })();
        // the remote file goes in every case; its failure cannot change the answer
        let _ = self.shell(&["rm", "-f", &remote]);
        res
    }

    fn tap(&mut self, node: &Node) -> Result<()> {
        let (x, y) = center(node)?;
        self.shell(&["input", "tap", &x.to_string(), &y.to_string()])?;
        Ok(())
    }

    fn long_press(&mut self, node: &Node) -> Result<()> {
        self.long_press_for(node, 800)
    }

    fn long_press_for(&mut self, node: &Node, ms: u32) -> Result<()> {
        let (x, y) = center(node)?;
        let (x, y) = (x.to_string(), y.to_string());
        self.shell(&["input", "swipe", &x, &y, &x, &y, &ms.to_string()])?;
        Ok(())
    }

    fn swipe(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, ms: u32) -> Result<()> {
        self.shell(&[
            "input",
            "swipe",
            &x1.to_string(),
            &y1.to_string(),
            &x2.to_string(),
            &y2.to_string(),
            &ms.to_string(),
        ])?;
        Ok(())
    }

    fn back(&mut self) -> Result<()> {
        self.shell(&["input", "keyevent", "4"])?;
        Ok(())
    }

    fn key(&mut self, code: &str) -> Result<()> {
        plain("key code", code, "")?;
        self.shell(&["input", "keyevent", code])?;
        Ok(())
    }

    fn start_app(&mut self, package: &str) -> Result<()> {
        plain("package", package, ".")?;
        let out = self.shell(&[
            "monkey",
            "-p",
            package,
            "-c",
            "android.intent.category.LAUNCHER",
            "1",
        ])?;
        if String::from_utf8_lossy(&out.stdout).contains("No activities found") {
            bail!("the app has no launcher activity");
        }
        Ok(())
    }

    fn start_activity(&mut self, component: &str) -> Result<()> {
        plain("component", component, "./$")?;
        // NEW_TASK | CLEAR_TASK: the component becomes the root of a fresh task of its app, so an app
        // left deep in its own screens comes back to its start (only the app's stack of screens is
        // reset, nothing of its data). An activity the app does not export is refused by `am` with
        // "Exception occurred" or "Error" (RT1, 2026-11-05: Threema's `.home.HomeActivity`).
        // `am` says a refusal on stderr: the device's shell folds it into stdout.
        let out = self.shell(&["am", "start", "-f", "0x10008000", "-n", component, "2>&1"])?;
        let said = String::from_utf8_lossy(&out.stdout);
        if out.code != 0 || said.contains("Error") || said.contains("Exception") {
            bail!("am start refused the component");
        }
        Ok(())
    }

    fn top_activity(&mut self) -> Result<String> {
        let out = self.shell(&["dumpsys", "activity", "activities"])?;
        let text = String::from_utf8_lossy(&out.stdout);
        for key in ["topResumedActivity", "mResumedActivity"] {
            for line in text.lines().filter(|l| l.contains(key)) {
                if let Some(tok) = line
                    .split_whitespace()
                    .map(|t| t.trim_end_matches('}'))
                    .find(|t| {
                        t.contains('/') && !t.starts_with('/') && t.chars().all(is_component_char)
                    })
                {
                    return Ok(tok.to_string());
                }
            }
        }
        bail!("no resumed activity found")
    }

    fn screenshot(&mut self, path: &Path) -> Result<()> {
        let out = self.run_ok(&["exec-out", "screencap", "-p"])?;
        if !out.stdout.starts_with(b"\x89PNG") {
            bail!("screencap did not return a PNG");
        }
        std::fs::write(path, &out.stdout).context("cannot write the screenshot")?;
        Ok(())
    }

    fn notifications(&mut self) -> Result<UiTree> {
        self.shell(&["cmd", "statusbar", "expand-notifications"])?;
        self.exec.sleep(Duration::from_millis(600));
        let res = self.dump();
        let _ = self.shell(&["cmd", "statusbar", "collapse"]);
        res
    }

    fn now(&self) -> Duration {
        self.exec.now()
    }

    fn sleep(&mut self, d: Duration) {
        self.exec.sleep(d);
    }

    fn shell_out(&mut self, tokens: &[&str]) -> Result<String> {
        if tokens.is_empty() {
            bail!("no shell command given");
        }
        for t in tokens {
            plain("shell token", t, ".:/=,-")?;
        }
        let out = self.shell(tokens)?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn root(&mut self, script: &super::RootScript) -> Result<String> {
        let line = script.line();
        // The lines are made from numbers and tags; this is the belt over the braces.
        if !line.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    ' ' | '_' | '.' | '/' | '-' | ':' | '=' | ';' | '&' | '>' | '"'
                )
        }) {
            bail!("the root script has characters that are not allowed on a shell line");
        }
        let quoted = format!("'{line}'");
        let out = self.shell(&["/debug_ramdisk/su", "-c", &quoted])?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn type_text(&mut self, text: &str) -> Result<()> {
        if text.is_empty()
            || !text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            bail!("the text to type has characters that are not allowed");
        }
        self.shell(&["input", "text", text])?;
        Ok(())
    }
}

fn is_component_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '_' | '$')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct Rec {
        calls: Vec<Vec<String>>,
        replies: VecDeque<(i32, Vec<u8>)>,
        clock: Duration,
    }

    impl Exec for Rec {
        fn run(&mut self, args: &[String]) -> Result<ExecOut> {
            self.calls.push(args.to_vec());
            let (code, stdout) = self.replies.pop_front().unwrap_or((0, Vec::new()));
            Ok(ExecOut { code, stdout })
        }
        fn now(&self) -> Duration {
            self.clock
        }
        fn sleep(&mut self, d: Duration) {
            self.clock += d;
        }
    }

    fn phone(serial: &str, replies: Vec<(i32, &[u8])>) -> AdbPhone<Rec> {
        let rec = Rec {
            replies: replies.into_iter().map(|(c, b)| (c, b.to_vec())).collect(),
            ..Default::default()
        };
        AdbPhone::with_exec(serial, rec).unwrap()
    }

    fn calls(p: &AdbPhone<Rec>) -> Vec<Vec<&str>> {
        p.exec()
            .calls
            .iter()
            .map(|c| c.iter().map(String::as_str).collect())
            .collect()
    }

    fn node(xml_bounds: &str) -> Node {
        let t = UiTree::parse(&format!(
            "<hierarchy><node bounds=\"{xml_bounds}\"/></hierarchy>"
        ))
        .unwrap();
        t.iter().next().unwrap().clone()
    }

    #[test]
    fn test_phone_adb_connect_lan_serial() {
        let mut p = phone(
            "10.0.0.2:5555",
            vec![(0, b"connected to 10.0.0.2:5555\n"), (0, b"device\n")],
        );
        p.connect().unwrap();
        assert_eq!(
            calls(&p),
            vec![
                vec!["connect", "10.0.0.2:5555"],
                vec!["-s", "10.0.0.2:5555", "get-state"]
            ]
        );
    }

    #[test]
    fn test_phone_adb_connect_usb_serial_skips_connect() {
        let mut p = phone("ABC123", vec![(0, b"device\n")]);
        p.connect().unwrap();
        assert_eq!(calls(&p), vec![vec!["-s", "ABC123", "get-state"]]);
    }

    #[test]
    fn test_phone_adb_connect_fails_closed() {
        let mut p = phone("h:1", vec![(0, b"failed to connect to h:1\n")]);
        assert!(p.connect().is_err());
        let mut p = phone(
            "h:1",
            vec![(0, b"already connected to h:1\n"), (0, b"offline\n")],
        );
        assert!(p.connect().is_err());
        let mut p = phone("h:1", vec![(0, b"connected to h:1\n"), (1, b"")]);
        assert!(p.connect().is_err());
    }

    #[test]
    fn test_phone_adb_shell_out_takes_plain_tokens_only() {
        let mut p = phone("S1", vec![(0, b"mWakefulness=Asleep\n")]);
        let out = p.shell_out(&["dumpsys", "power"]).unwrap();
        assert!(out.contains("Asleep"));
        assert_eq!(
            calls(&p),
            vec![vec!["-s", "S1", "shell", "dumpsys", "power"]]
        );
        for bad in ["a;b", "a b", "$(x)", "a'b", "", "a|b"] {
            assert!(p.shell_out(&["x", bad]).is_err(), "{bad:?}");
        }
        assert!(p.shell_out(&[]).is_err());
        assert_eq!(p.exec().calls.len(), 1, "a refused token ran nothing");
    }

    #[test]
    fn test_phone_adb_root_runs_only_the_closed_set_of_fixed_lines() {
        use super::super::{CutTag, RootScript, Table};
        let mut p = phone("S1", vec![(0, b"ok\n"), (0, b""), (0, b"")]);
        p.root(&RootScript::ListOutput(Table::V4)).unwrap();
        let tag = CutTag::new("abcdef").unwrap();
        p.root(&RootScript::DeleteDrop {
            table: Table::V6,
            uid: 10194,
            tag: tag.clone(),
        })
        .unwrap();
        p.root(&RootScript::CutOff {
            uid: 10194,
            tag,
            seconds: 600,
        })
        .unwrap();
        let c = calls(&p);
        assert_eq!(
            c[0],
            vec![
                "-s",
                "S1",
                "shell",
                "/debug_ramdisk/su",
                "-c",
                "'iptables -w -S OUTPUT'"
            ]
        );
        assert!(c[1][5].starts_with("'ip6tables -w -D OUTPUT -m owner --uid-owner 10194 -m comment --comment kourier-cut-abcdef -j DROP"));
        assert!(c[2][5].contains("sleep 600; iptables -w -D OUTPUT"));
        assert!(c[2][5].contains("nohup sh -c"));
        // a nonce is six lower-case letters: nothing else reaches a root line
        for bad in [
            "abcde", "abcdefg", "abc;ef", "ABCDEF", "abc ef", "abc'ef", "",
        ] {
            assert!(CutTag::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn test_phone_adb_type_text_is_a_plain_word() {
        let mut p = phone("S1", vec![]);
        p.type_text("kourier-rt1-mirror").unwrap();
        assert_eq!(
            calls(&p),
            vec![vec![
                "-s",
                "S1",
                "shell",
                "input",
                "text",
                "kourier-rt1-mirror"
            ]]
        );
        for bad in ["", "a b", "a%sb", "a;b", "é"] {
            assert!(p.type_text(bad).is_err(), "{bad:?}");
        }
        assert_eq!(p.exec().calls.len(), 1);
    }

    #[test]
    fn test_phone_adb_rejects_bad_serials() {
        for s in ["", "-a", "a b"] {
            assert!(AdbPhone::with_exec(s, Rec::default()).is_err(), "{s:?}");
        }
    }

    #[test]
    fn test_phone_adb_wake_taps_swipes_keys() {
        let mut p = phone("S", vec![]);
        p.wake().unwrap();
        let n = node("[10,20][110,60]");
        p.tap(&n).unwrap();
        p.long_press(&n).unwrap();
        p.long_press_for(&n, 2000).unwrap();
        p.swipe(1, 2, 3, 4, 250).unwrap();
        p.back().unwrap();
        p.key("KEYCODE_ENTER").unwrap();
        p.start_app("ch.threema.app").unwrap();
        p.start_activity("ch.threema.app/.activities.HomeActivity")
            .unwrap();
        let s = ["-s", "S", "shell"];
        let with = |tail: &[&'static str]| -> Vec<&str> { s.iter().chain(tail).copied().collect() };
        assert_eq!(
            calls(&p),
            vec![
                with(&["input", "keyevent", "KEYCODE_WAKEUP"]),
                with(&["wm", "dismiss-keyguard"]),
                with(&["input", "tap", "60", "40"]),
                with(&["input", "swipe", "60", "40", "60", "40", "800"]),
                with(&["input", "swipe", "60", "40", "60", "40", "2000"]),
                with(&["input", "swipe", "1", "2", "3", "4", "250"]),
                with(&["input", "keyevent", "4"]),
                with(&["input", "keyevent", "KEYCODE_ENTER"]),
                with(&[
                    "monkey",
                    "-p",
                    "ch.threema.app",
                    "-c",
                    "android.intent.category.LAUNCHER",
                    "1"
                ]),
                with(&[
                    "am",
                    "start",
                    "-f",
                    "0x10008000",
                    "-n",
                    "ch.threema.app/.activities.HomeActivity",
                    "2>&1"
                ]),
            ]
        );
    }

    #[test]
    fn test_phone_adb_refuses_shell_metacharacters_and_empty_bounds() {
        let mut p = phone("S", vec![]);
        assert!(p.key("4; reboot").is_err());
        assert!(p.start_app("a b").is_err());
        assert!(p.start_app("a;b").is_err());
        assert!(p.start_activity("a/b && x").is_err());
        assert!(p.tap(&node("[5,5][5,9]")).is_err());
        assert!(p.exec().calls.is_empty());
    }

    #[test]
    fn test_phone_adb_failed_command_is_an_error() {
        let mut p = phone("S", vec![(1, b"")]);
        assert!(p.back().is_err());
        let mut p = phone("S", vec![(0, b"** No activities found; aborting\n")]);
        assert!(p.start_app("a.b").is_err());
    }

    #[test]
    fn test_phone_adb_dump_flow_reads_in_memory_and_removes_the_file() {
        let xml = b"<hierarchy><node bounds=\"[0,0][9,9]\" text=\"x\"/></hierarchy>";
        let mut p = phone(
            "S",
            vec![
                (0, b"UI hierarchy dumped to: /sdcard/x.xml\n"),
                (0, xml),
                (0, b""),
            ],
        );
        let t = p.dump().unwrap();
        assert_eq!(t.len(), 1);
        let c = calls(&p);
        assert_eq!(c.len(), 3);
        let remote = c[0][5];
        assert!(remote.starts_with("/sdcard/kourier-ui-") && remote.ends_with(".xml"));
        assert_eq!(c[0][..5], ["-s", "S", "shell", "uiautomator", "dump"]);
        assert_eq!(c[1], vec!["-s", "S", "exec-out", "cat", remote]);
        assert_eq!(c[2], vec!["-s", "S", "shell", "rm", "-f", remote]);
        // a second dump uses another file name
        let mut p2 = phone("S", vec![]);
        let _ = p2.dump();
        assert_ne!(p2.exec().calls[0][5], remote);
    }

    #[test]
    fn test_phone_adb_dump_failures_fail_closed_and_still_remove_the_file() {
        // uiautomator says ERROR
        let mut p = phone(
            "S",
            vec![(
                0,
                b"ERROR: null root node returned by UiTestAutomationBridge.\n",
            )],
        );
        assert!(p.dump().is_err());
        assert_eq!(calls(&p).last().unwrap()[3], "rm");
        // cat returns garbage
        let mut p = phone("S", vec![(0, b"dumped"), (0, b"not xml"), (0, b"")]);
        assert!(p.dump().is_err());
        assert_eq!(calls(&p).last().unwrap()[3], "rm");
        // cat fails
        let mut p = phone("S", vec![(0, b"dumped"), (1, b""), (0, b"")]);
        assert!(p.dump().is_err());
        assert_eq!(calls(&p).len(), 3);
    }

    #[test]
    fn test_phone_adb_notifications_expand_dump_collapse() {
        let xml = b"<hierarchy><node bounds=\"[0,0][9,9]\"/></hierarchy>";
        let mut p = phone(
            "S",
            vec![(0, b""), (0, b"dumped"), (0, xml), (0, b""), (0, b"")],
        );
        let t = p.notifications().unwrap();
        assert_eq!(t.len(), 1);
        let c = calls(&p);
        assert_eq!(
            c[0],
            vec![
                "-s",
                "S",
                "shell",
                "cmd",
                "statusbar",
                "expand-notifications"
            ]
        );
        assert_eq!(
            c.last().unwrap().as_slice(),
            ["-s", "S", "shell", "cmd", "statusbar", "collapse"]
        );
        // collapse also after a failed dump
        let mut p = phone("S", vec![(0, b""), (0, b"ERROR")]);
        assert!(p.notifications().is_err());
        assert_eq!(calls(&p).last().unwrap().last(), Some(&"collapse"));
    }

    #[test]
    fn test_phone_adb_top_activity_returns_the_component_only() {
        let out = b"  mResumedActivity: ActivityRecord{1a2b u0 ch.threema.app/.activities.HomeActivity t12}\n";
        let mut p = phone("S", vec![(0, out)]);
        assert_eq!(
            p.top_activity().unwrap(),
            "ch.threema.app/.activities.HomeActivity"
        );
        let mut p = phone("S", vec![(0, b"nothing here\n")]);
        assert!(p.top_activity().is_err());
    }

    #[test]
    fn test_phone_adb_screenshot_command_and_png_check() {
        let dir = std::env::temp_dir().join(format!("tp-shot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.png");
        let mut p = phone("S", vec![(0, b"\x89PNG....")]);
        p.screenshot(&f).unwrap();
        assert_eq!(
            calls(&p),
            vec![vec!["-s", "S", "exec-out", "screencap", "-p"]]
        );
        assert_eq!(std::fs::read(&f).unwrap(), b"\x89PNG....");
        let mut p = phone("S", vec![(0, b"junk")]);
        assert!(p.screenshot(&dir.join("b.png")).is_err());
        assert!(!dir.join("b.png").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_phone_adb_clock_is_the_execs() {
        let mut p = phone("S", vec![]);
        p.sleep(Duration::from_secs(2));
        assert_eq!(p.now(), Duration::from_secs(2));
    }
}
