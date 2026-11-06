//! The TEST PHONE layer of the run tools: drive the user's test phone over adb, read its screen as a
//! tree, and answer a run tool's y/n questions by CHECKS instead of asking the user. Generic: nothing
//! here knows Threema or WhatsApp (their screens, ids and flows live in `threema_run` / `wa_run`).
//!
//! Parts: [`Phone`] (the trait), [`AdbPhone`] (the real one, over an [`Exec`] so the command lines are
//! testable), [`FakePhone`] (scripted screens for tests), [`UiTree`] / [`Node`] (the parsed dump),
//! [`Secret`] (a text read from the phone), and [`wait_for`] / [`tap_and_wait`] (polling that fails
//! closed). See `target/threema/phone/test-phone.md` for the long form.
//!
//! RULES. (1) Fail closed: an unreadable screen, a missing node or a tap that did not take effect is
//! an `Err`, which a tool must report as a "no", never as a pass. (2) The serial is a value the tool
//! passes in (`--test-phone <serial>` or a profile value); nothing here names a device. (3) Texts the
//! phone shows are data: use [`Secret`], see its doc; errors and log lines of this module never carry
//! a node's text. The raw dump XML lives in memory only. (4) `screenshot` writes a picture of
//! whatever is on screen: the CALLER decides which screens are safe to picture (never a screen
//! showing a contact list, a chat, a code or a key).

#![allow(dead_code, unused_imports)]

mod adb;
mod fake;
mod secret;
mod tree;

pub use adb::{AdbPhone, Exec, ExecOut, ProcessExec};
pub use fake::{Act, FakePhone, FakeScreen, NodeSpec, ShellHandler};
pub use secret::Secret;
pub use tree::{Bounds, Node, ParseError, UiTree};

use anyhow::{Result, bail};
use std::path::Path;
use std::time::Duration;

/// How often `wait_for` re-reads the screen.
pub const POLL_INTERVAL: Duration = Duration::from_millis(300);
/// How often `tap_and_wait` taps before it gives up.
pub const TAP_ATTEMPTS: u32 = 3;

/// An iptables table of the firewall scripts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Table {
    V4,
    V6,
}

impl Table {
    pub fn program(self) -> &'static str {
        match self {
            Table::V4 => "iptables",
            Table::V6 => "ip6tables",
        }
    }
}

/// The tag of one cut's rules: `kourier-cut-<six lower-case letters>` as an iptables comment, so a
/// timer or a removal touches only the rules of its own cut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CutTag(String);

impl CutTag {
    /// Six lower-case letters, nothing else.
    pub fn new(nonce: &str) -> Result<CutTag> {
        if nonce.len() != 6 || !nonce.bytes().all(|b| b.is_ascii_lowercase()) {
            bail!("a cut's nonce is six lower-case letters");
        }
        Ok(CutTag(format!("kourier-cut-{nonce}")))
    }
    pub fn comment(&self) -> &str {
        &self.0
    }
}

/// The root scripts a phone may run: a closed set of fixed lines (the run tools' firewall cut of one
/// app's user id). Every variable part is a number or a [`CutTag`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootScript {
    /// `<table> -w -S OUTPUT`
    ListOutput(Table),
    /// Insert the DROP rules of `uid` carrying `tag` in both tables, and start, in the same root
    /// shell, the timer that deletes only those tagged rules after `seconds`.
    CutOff { uid: u32, tag: CutTag, seconds: u32 },
    /// Delete one DROP rule of `uid` carrying `tag` from `table`.
    DeleteDrop { table: Table, uid: u32, tag: CutTag },
}

impl RootScript {
    fn rule(table: Table, flag: &str, uid: u32, tag: &CutTag) -> String {
        format!(
            "{} -w {flag} OUTPUT -m owner --uid-owner {uid} -m comment --comment {} -j DROP",
            table.program(),
            tag.comment()
        )
    }

    /// The root shell line (letters, digits, spaces and `_ . / - : = ; & > "` only).
    pub fn line(&self) -> String {
        match self {
            RootScript::ListOutput(t) => format!("{} -w -S OUTPUT", t.program()),
            RootScript::CutOff { uid, tag, seconds } => format!(
                "{}; {}; nohup sh -c \"sleep {seconds}; {}; {}\" >/dev/null 2>&1 &",
                Self::rule(Table::V4, "-I", *uid, tag),
                Self::rule(Table::V6, "-I", *uid, tag),
                Self::rule(Table::V4, "-D", *uid, tag),
                Self::rule(Table::V6, "-D", *uid, tag),
            ),
            RootScript::DeleteDrop { table, uid, tag } => Self::rule(*table, "-D", *uid, tag),
        }
    }
}

pub trait Phone {
    /// `adb connect <serial>` when the serial is `host:port`, then `adb -s <serial> get-state`
    /// must say `device`.
    fn connect(&mut self) -> Result<()>;
    /// Key WAKEUP, then dismiss the keyguard (the phone dozes between steps).
    fn wake(&mut self) -> Result<()>;
    /// Read the screen. The raw XML is never written or logged.
    fn dump(&mut self) -> Result<UiTree>;
    fn tap(&mut self, node: &Node) -> Result<()>;
    fn long_press(&mut self, node: &Node) -> Result<()>;
    /// A press held for `ms` milliseconds (WhatsApp's chat list needs about 2000; `long_press` holds 800).
    /// Provided, so an implementor that has no better hold is a plain long press (the fake treats any
    /// hold as a long press).
    fn long_press_for(&mut self, node: &Node, _ms: u32) -> Result<()> {
        self.long_press(node)
    }
    fn swipe(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, ms: u32) -> Result<()>;
    fn back(&mut self) -> Result<()>;
    /// A keyevent by name (`KEYCODE_ENTER`) or number (`66`).
    fn key(&mut self, code: &str) -> Result<()>;
    /// Start an app by package through its launcher activity.
    fn start_app(&mut self, package: &str) -> Result<()>;
    /// `am start -f 0x10008000 -n <package>/<activity>`: the activity as the root of a fresh task of its
    /// app (its stack of screens reset, none of its data); an activity the app does not export is an error.
    fn start_activity(&mut self, component: &str) -> Result<()>;
    /// `package/activity` of the resumed activity, nothing else.
    fn top_activity(&mut self) -> Result<String>;
    /// A PNG of the screen into `path`. The caller decides which screens may be pictured.
    fn screenshot(&mut self, path: &Path) -> Result<()>;
    /// The expanded notification shade as a tree (it is collapsed again afterwards).
    fn notifications(&mut self) -> Result<UiTree>;
    /// Time since this phone object was made (a virtual clock in the fake).
    fn now(&self) -> Duration;
    fn sleep(&mut self, d: Duration);

    // ---- Added for the run tools' automated paths. They have defaults that refuse, so a Phone
    // that only reads a screen (and every earlier implementation) stays as it was.

    /// `adb shell <tokens>` and its stdout as text. Every token is a plain word (letters, digits and
    /// `_ . / - : = ,`): nothing the device's shell could split or expand. The CALLER parses what it
    /// needs and drops the rest; a tool never prints or logs this text.
    fn shell_out(&mut self, tokens: &[&str]) -> Result<String> {
        let _ = tokens;
        bail!("this phone runs no shell commands")
    }
    /// Runs one of the FIXED root scripts ([`RootScript`]: no free text can reach a root shell) as
    /// `adb shell /debug_ramdisk/su -c '<line>'` and returns stdout.
    fn root(&mut self, script: &RootScript) -> Result<String> {
        let _ = script;
        bail!("this phone runs no root shell commands")
    }
    /// `input text` of a fixed ASCII word (letters, digits, `-` `_` `.` only).
    fn type_text(&mut self, text: &str) -> Result<()> {
        let _ = text;
        bail!("this phone types no text")
    }
}

/// Poll `dump()` until `pred` holds. A failed dump counts as "not yet" (a screen in transition may
/// not dump) but never as a pass; at the timeout the result is an error.
pub fn wait_for<P: Phone + ?Sized>(
    phone: &mut P,
    timeout: Duration,
    mut pred: impl FnMut(&UiTree) -> bool,
) -> Result<UiTree> {
    let start = phone.now();
    let mut dumps = 0u32;
    let mut failed = 0u32;
    loop {
        match phone.dump() {
            Ok(t) => {
                dumps += 1;
                if pred(&t) {
                    return Ok(t);
                }
            }
            Err(_) => failed += 1,
        }
        if phone.now().saturating_sub(start) >= timeout {
            bail!(
                "the screen did not reach the expected state within {} ms ({dumps} dumps read, {failed} failed)",
                timeout.as_millis()
            );
        }
        phone.sleep(POLL_INTERVAL);
    }
}

/// Tap `node`, then wait for `pred` on the next screens. A tap can be refused or land late, so up to
/// [`TAP_ATTEMPTS`] taps are made, each followed by `timeout / TAP_ATTEMPTS` of polling. The node is
/// the one from the dump before the tap: the same place is tapped again. An error, never a pass,
/// when the predicate never holds.
pub fn tap_and_wait<P: Phone + ?Sized>(
    phone: &mut P,
    node: &Node,
    mut pred: impl FnMut(&UiTree) -> bool,
    timeout: Duration,
) -> Result<UiTree> {
    let per = timeout / TAP_ATTEMPTS;
    let mut tap_errors = 0u32;
    for attempt in 0..TAP_ATTEMPTS {
        // A retry only while the screen still shows the node where it was: a tap that took effect
        // but led elsewhere must not be repeated on whatever the new screen has at that place (an
        // archive, a send, a toggle would run twice or hit another control).
        if attempt > 0 {
            let Ok(now) = phone.dump() else {
                bail!("the screen could not be read before a second tap: no second tap was made");
            };
            if !now.still_shows(node) {
                bail!(
                    "the screen changed after the tap, but not to the expected state: no second tap was made"
                );
            }
        }
        if phone.tap(node).is_err() {
            tap_errors += 1;
            phone.sleep(POLL_INTERVAL);
            continue;
        }
        if let Ok(t) = wait_for(phone, per, &mut pred) {
            return Ok(t);
        }
    }
    bail!(
        "tap had no effect after {TAP_ATTEMPTS} attempts ({tap_errors} refused, {} ms each wait)",
        per.as_millis()
    )
}

/// Find the single node `select` names on the current screen and tap it, waiting for `pred`.
pub fn dump_tap_and_wait<P: Phone + ?Sized>(
    phone: &mut P,
    select: impl Fn(&UiTree) -> Option<Node>,
    pred: impl FnMut(&UiTree) -> bool,
    timeout: Duration,
) -> Result<UiTree> {
    let tree = phone.dump()?;
    let Some(node) = select(&tree) else {
        bail!("the node to tap is not on the screen");
    };
    drop(tree);
    tap_and_wait(phone, &node, pred, timeout)
}

/// `Node` clone for `dump_tap_and_wait`'s selector: `tree.by_id(..).first().map(|n| (*n).clone())`.
pub fn first_of(nodes: Vec<&Node>) -> Option<Node> {
    nodes.first().map(|n| (*n).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_screens() -> FakePhone {
        FakePhone::new("home")
            .screen(
                FakeScreen::new("home")
                    .node(NodeSpec::new().id("t:id/go").text("Go").clickable())
                    .on_tap_id("t:id/go", "next"),
            )
            .screen(FakeScreen::new("next").node(NodeSpec::new().id("t:id/done")))
    }

    #[test]
    fn test_phone_wait_for_succeeds_and_times_out_on_virtual_clock() {
        let mut p = two_screens();
        let t = wait_for(&mut p, Duration::from_secs(5), |t| t.has_id("t:id/go")).unwrap();
        assert!(t.has_id("t:id/go"));
        let before = p.now();
        let e = wait_for(&mut p, Duration::from_secs(2), |t| t.has_id("t:id/never")).unwrap_err();
        assert!(e.to_string().contains("did not reach"));
        let spent = p.now() - before;
        assert!(
            spent >= Duration::from_secs(2) && spent < Duration::from_secs(3),
            "{spent:?}"
        );
    }

    #[test]
    fn test_phone_wait_for_unreadable_screen_never_passes() {
        let mut p = FakePhone::new("x").screen(FakeScreen::unreadable("x"));
        // even a predicate that is always true cannot pass without a tree
        let e = wait_for(&mut p, Duration::from_secs(1), |_| true).unwrap_err();
        assert!(e.to_string().contains("0 dumps read"), "{e}");
    }

    #[test]
    fn test_phone_tap_and_wait_follows_the_transition() {
        let mut p = two_screens();
        let t = p.dump().unwrap();
        let go = t.require_id("t:id/go").unwrap().clone();
        let t = tap_and_wait(
            &mut p,
            &go,
            |t| t.has_id("t:id/done"),
            Duration::from_secs(6),
        )
        .unwrap();
        assert!(t.has_id("t:id/done"));
        assert_eq!(p.taps(), 1);
    }

    #[test]
    fn test_phone_tap_and_wait_retries_an_ignored_tap() {
        let mut p = two_screens();
        p.ignore_taps("home", 2);
        let go = first_of(p.dump().unwrap().by_id("t:id/go")).unwrap();
        tap_and_wait(
            &mut p,
            &go,
            |t| t.has_id("t:id/done"),
            Duration::from_secs(6),
        )
        .unwrap();
        assert_eq!(p.taps(), 3);
    }

    #[test]
    fn test_phone_tap_and_wait_fails_when_every_tap_is_ignored() {
        let mut p = two_screens();
        p.ignore_taps("home", 99);
        let go = first_of(p.dump().unwrap().by_id("t:id/go")).unwrap();
        let e = tap_and_wait(
            &mut p,
            &go,
            |t| t.has_id("t:id/done"),
            Duration::from_secs(6),
        )
        .unwrap_err();
        assert!(e.to_string().contains("no effect"), "{e}");
        assert_eq!(p.taps(), TAP_ATTEMPTS as usize);
    }

    #[test]
    fn test_phone_tap_and_wait_handles_a_late_landing() {
        let mut p = two_screens();
        p.land_late("home", 4);
        let go = first_of(p.dump().unwrap().by_id("t:id/go")).unwrap();
        tap_and_wait(
            &mut p,
            &go,
            |t| t.has_id("t:id/done"),
            Duration::from_secs(9),
        )
        .unwrap();
    }

    #[test]
    fn test_phone_tap_and_wait_never_taps_again_on_a_changed_screen() {
        // The tap took effect but led to a screen that is not the expected one, with another
        // control at the same place: one tap, then an error, never a second tap there.
        let mut p = FakePhone::new("home")
            .screen(
                FakeScreen::new("home")
                    .node(NodeSpec::new().id("t:id/go").text("Go").clickable())
                    .on_tap_id("t:id/go", "elsewhere"),
            )
            .screen(
                FakeScreen::new("elsewhere")
                    .node(NodeSpec::new().id("t:id/delete").text("Delete").clickable()),
            );
        let go = first_of(p.dump().unwrap().by_id("t:id/go")).unwrap();
        let e = tap_and_wait(
            &mut p,
            &go,
            |t| t.has_id("t:id/done"),
            Duration::from_secs(6),
        )
        .unwrap_err();
        assert!(e.to_string().contains("no second tap"), "{e}");
        assert_eq!(p.taps(), 1);
    }

    #[test]
    fn test_phone_long_press_for_is_a_long_press_in_the_fake() {
        let mut p = FakePhone::new("home")
            .screen(
                FakeScreen::new("home")
                    .node(NodeSpec::new().id("t:id/row").text("Row").clickable())
                    .on_long_press_id("t:id/row", "menu"),
            )
            .screen(FakeScreen::new("menu").node(NodeSpec::new().id("t:id/menu")));
        let row = first_of(p.dump().unwrap().by_id("t:id/row")).unwrap();
        p.long_press_for(&row, 2000).unwrap();
        assert_eq!(p.current(), "menu");
    }

    #[test]
    fn test_phone_dump_tap_and_wait_missing_node_is_an_error() {
        let mut p = two_screens();
        let e = dump_tap_and_wait(
            &mut p,
            |t| first_of(t.by_id("t:id/absent")),
            |_| true,
            Duration::from_secs(3),
        )
        .unwrap_err();
        assert!(e.to_string().contains("not on the screen"));
        assert_eq!(p.taps(), 0);
    }
}
