//! A scripted fake phone for the tests of the run tools: named screens, transitions on tap, long
//! press and back, a log of the acts, and the failure modes a caller must fail closed on.
//!
//! ```ignore
//! let mut p = FakePhone::new("chat")
//!     .screen(FakeScreen::new("chat")
//!         .node(NodeSpec::new().id("app:id/menu").clickable())
//!         .on_tap_id("app:id/menu", "menu"))
//!     .screen(FakeScreen::new("menu").node(NodeSpec::new().id("app:id/about").text("About")))
//!     .screen(FakeScreen::unreadable("locked"));
//! p.ignore_taps("chat", 1);   // the first tap on this screen is swallowed
//! p.land_late("chat", 3);     // a tap takes effect only after 3 more dumps
//! ```
//!
//! A screen is a builder ([`NodeSpec`]) or an XML string ([`FakeScreen::from_xml`]); both end up as
//! the same uiautomator-shaped text that the real parser reads, so the fake exercises the parser too.
//! Time is virtual: `sleep` advances `now`, so timeouts cost nothing.

use super::Phone;
use super::tree::{Node, UiTree};
use anyhow::{Result, bail};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Default)]
pub struct NodeSpec {
    id: String,
    text: String,
    desc: String,
    class: String,
    package: String,
    clickable: bool,
    checked: bool,
    bounds: Option<(i32, i32, i32, i32)>,
    children: Vec<NodeSpec>,
}

impl NodeSpec {
    pub fn new() -> NodeSpec {
        NodeSpec {
            class: "android.view.View".into(),
            package: "fake.app".into(),
            ..Default::default()
        }
    }
    pub fn id(mut self, v: &str) -> Self {
        self.id = v.into();
        self
    }
    pub fn text(mut self, v: &str) -> Self {
        self.text = v.into();
        self
    }
    pub fn desc(mut self, v: &str) -> Self {
        self.desc = v.into();
        self
    }
    pub fn class(mut self, v: &str) -> Self {
        self.class = v.into();
        self
    }
    pub fn package(mut self, v: &str) -> Self {
        self.package = v.into();
        self
    }
    pub fn clickable(mut self) -> Self {
        self.clickable = true;
        self
    }
    pub fn checked(mut self) -> Self {
        self.checked = true;
        self
    }
    pub fn bounds(mut self, l: i32, t: i32, r: i32, b: i32) -> Self {
        self.bounds = Some((l, t, r, b));
        self
    }
    pub fn child(mut self, c: NodeSpec) -> Self {
        self.children.push(c);
        self
    }

    fn write(&self, out: &mut String, seq: &mut i32) {
        let (l, t, r, b) = self.bounds.unwrap_or_else(|| {
            let t = *seq * 100;
            (0, t, 1080, t + 90)
        });
        *seq += 1;
        out.push_str(&format!(
            "<node index=\"0\" text=\"{}\" resource-id=\"{}\" class=\"{}\" package=\"{}\" content-desc=\"{}\" \
             checked=\"{}\" clickable=\"{}\" enabled=\"true\" bounds=\"[{l},{t}][{r},{b}]\"",
            esc(&self.text),
            esc(&self.id),
            esc(&self.class),
            esc(&self.package),
            esc(&self.desc),
            self.checked,
            self.clickable
        ));
        if self.children.is_empty() {
            out.push_str("/>");
        } else {
            out.push('>');
            for c in &self.children {
                c.write(out, seq);
            }
            out.push_str("</node>");
        }
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn xml_of(nodes: &[NodeSpec]) -> String {
    let mut s = String::from(
        "<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation=\"0\">",
    );
    let mut seq = 0;
    for n in nodes {
        n.write(&mut s, &mut seq);
    }
    s.push_str("</hierarchy>");
    s
}

#[derive(Clone)]
enum Sel {
    Id(String),
    Text(String),
}

impl Sel {
    fn matches(&self, n: &Node) -> bool {
        match self {
            Sel::Id(i) => n.resource_id() == i,
            Sel::Text(t) => n.text_is(t),
        }
    }
}

#[derive(Clone)]
pub struct FakeScreen {
    name: String,
    specs: Vec<NodeSpec>,
    raw_xml: Option<String>,
    unreadable: bool,
    activity: String,
    taps: Vec<(Sel, String)>,
    long_presses: Vec<(Sel, String)>,
    keys: Vec<(String, String)>,
    swipe_up_to: Option<String>,
    type_to: Option<String>,
    back_to: Option<String>,
}

impl FakeScreen {
    pub fn new(name: &str) -> FakeScreen {
        FakeScreen {
            name: name.into(),
            specs: Vec::new(),
            raw_xml: None,
            unreadable: false,
            activity: "fake.app/.Main".into(),
            taps: Vec::new(),
            long_presses: Vec::new(),
            keys: Vec::new(),
            swipe_up_to: None,
            type_to: None,
            back_to: None,
        }
    }
    /// A screen whose nodes are missing: whatever a caller looks for is not there.
    pub fn blank(name: &str) -> FakeScreen {
        FakeScreen::new(name)
    }
    /// A screen whose `dump()` fails (uiautomator could not read it).
    pub fn unreadable(name: &str) -> FakeScreen {
        FakeScreen {
            unreadable: true,
            ..FakeScreen::new(name)
        }
    }
    pub fn from_xml(name: &str, xml: &str) -> FakeScreen {
        FakeScreen {
            raw_xml: Some(xml.into()),
            ..FakeScreen::new(name)
        }
    }
    pub fn node(mut self, n: NodeSpec) -> Self {
        self.specs.push(n);
        self
    }
    pub fn activity(mut self, component: &str) -> Self {
        self.activity = component.into();
        self
    }
    pub fn on_tap_id(mut self, id: &str, to: &str) -> Self {
        self.taps.push((Sel::Id(id.into()), to.into()));
        self
    }
    pub fn on_tap_text(mut self, text: &str, to: &str) -> Self {
        self.taps.push((Sel::Text(text.into()), to.into()));
        self
    }
    pub fn on_long_press_id(mut self, id: &str, to: &str) -> Self {
        self.long_presses.push((Sel::Id(id.into()), to.into()));
        self
    }
    pub fn on_long_press_text(mut self, text: &str, to: &str) -> Self {
        self.long_presses.push((Sel::Text(text.into()), to.into()));
        self
    }
    /// A keyevent (`KEYCODE_SLEEP`, ...) on this screen leads to `to`.
    pub fn on_key(mut self, code: &str, to: &str) -> Self {
        self.keys.push((code.into(), to.into()));
        self
    }
    /// A swipe UP (the finger moves up: the list scrolls down) on this screen leads to `to`.
    pub fn on_swipe_up(mut self, to: &str) -> Self {
        self.swipe_up_to = Some(to.into());
        self
    }
    /// Typing text on this screen leads to `to` (the field now shows it).
    pub fn on_type(mut self, to: &str) -> Self {
        self.type_to = Some(to.into());
        self
    }
    pub fn on_back(mut self, to: &str) -> Self {
        self.back_to = Some(to.into());
        self
    }
    fn xml(&self) -> String {
        self.raw_xml.clone().unwrap_or_else(|| xml_of(&self.specs))
    }
}

/// What the phone was asked to do. Carries ids and names, never texts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    Connect,
    Wake,
    Dump,
    Tap(String),
    LongPress(String),
    Swipe(i32, i32, i32, i32, u32),
    Back,
    Key(String),
    StartApp(String),
    StartActivity(String),
    TopActivity,
    Screenshot,
    Notifications,
    /// A `shell_out` call: its first token only.
    Shell(String),
    /// A `root` call: the first word of its line only.
    Root(String),
    /// A `type_text` call (the text is in [`FakePhone::typed`], never in the act).
    TypeText,
}

/// What a scripted shell answers: `(is_root, line)` in, stdout out. A test keeps its own state in it.
pub type ShellHandler = Box<dyn FnMut(bool, &str) -> Result<String>>;

pub struct FakePhone {
    screens: HashMap<String, FakeScreen>,
    current: String,
    pending: Option<(String, u32)>,
    ignore: HashMap<String, u32>,
    late: HashMap<String, u32>,
    starts: Vec<(String, String)>,
    shade: Option<Vec<NodeSpec>>,
    shade_unreadable: bool,
    connect_fails: bool,
    clock: Duration,
    acts: Vec<Act>,
    shell: Option<ShellHandler>,
    typed: Vec<String>,
    /// (visited, target, source): once `visited` is the current screen, screen `target` becomes a
    /// copy of `source` (a sent message stays in its chat).
    replace_on_visit: Vec<(String, String, String)>,
}

fn label(n: &Node) -> String {
    if n.resource_id().is_empty() {
        format!("#{}", n.index())
    } else {
        n.resource_id().to_string()
    }
}

impl FakePhone {
    pub fn new(start: &str) -> FakePhone {
        FakePhone {
            screens: HashMap::new(),
            current: start.into(),
            pending: None,
            ignore: HashMap::new(),
            late: HashMap::new(),
            starts: Vec::new(),
            shade: None,
            shade_unreadable: false,
            connect_fails: false,
            clock: Duration::ZERO,
            acts: Vec::new(),
            shell: None,
            typed: Vec::new(),
            replace_on_visit: Vec::new(),
        }
    }
    /// The phone's shell: `handler(is_root, line)` answers `shell_out` (the tokens joined by
    /// spaces) and `root` (the script's line). Without one both refuse, as a Phone that has no shell does.
    pub fn with_shell(
        mut self,
        handler: impl FnMut(bool, &str) -> Result<String> + 'static,
    ) -> Self {
        self.shell = Some(Box::new(handler));
        self
    }
    /// Once the phone has been on screen `visited`, screen `target` shows what `source` shows (a text
    /// sent from `visited` stays in the chat that is opened again later).
    pub fn on_visit_replace(mut self, visited: &str, target: &str, source: &str) -> Self {
        self.replace_on_visit
            .push((visited.into(), target.into(), source.into()));
        self
    }
    /// The words `type_text` got (test code only: the acts never carry them).
    pub fn typed(&self) -> &[String] {
        &self.typed
    }
    pub fn screen(mut self, s: FakeScreen) -> Self {
        self.screens.insert(s.name.clone(), s);
        self
    }
    /// Starting package `pkg` (or activity component) switches to screen `to`.
    pub fn on_start(mut self, pkg_or_component: &str, to: &str) -> Self {
        self.starts.push((pkg_or_component.into(), to.into()));
        self
    }
    pub fn with_shade(mut self, nodes: Vec<NodeSpec>) -> Self {
        self.shade = Some(nodes);
        self
    }
    pub fn with_unreadable_shade(mut self) -> Self {
        self.shade_unreadable = true;
        self
    }
    pub fn failing_connect(mut self) -> Self {
        self.connect_fails = true;
        self
    }
    /// Swallow the next `n` taps made on this screen (accepted, no effect).
    pub fn ignore_taps(&mut self, screen: &str, n: u32) {
        self.ignore.insert(screen.into(), n);
    }
    /// A tap on this screen takes effect only after `dumps` more dumps.
    pub fn land_late(&mut self, screen: &str, dumps: u32) {
        self.late.insert(screen.into(), dumps);
    }
    pub fn set_screen(&mut self, name: &str) {
        self.current = name.into();
        self.pending = None;
    }
    pub fn current(&self) -> &str {
        &self.current
    }
    pub fn acts(&self) -> &[Act] {
        &self.acts
    }
    pub fn taps(&self) -> usize {
        self.acts
            .iter()
            .filter(|a| matches!(a, Act::Tap(_)))
            .count()
    }

    fn cur(&self) -> Result<&FakeScreen> {
        match self.screens.get(&self.current) {
            Some(s) => Ok(s),
            None => bail!("the fake phone has no screen {}", self.current),
        }
    }

    fn arrive(&mut self) {
        let hits: Vec<(String, String)> = self
            .replace_on_visit
            .iter()
            .filter(|(v, _, _)| *v == self.current)
            .map(|(_, t, s)| (t.clone(), s.clone()))
            .collect();
        for (target, source) in hits {
            if let Some(src) = self.screens.get(&source).cloned() {
                let mut copy = src;
                copy.name = target.clone();
                self.screens.insert(target, copy);
            }
        }
    }

    fn settle(&mut self) {
        if let Some((t, n)) = self.pending.take() {
            if n == 0 {
                self.current = t;
                self.arrive();
            } else {
                self.pending = Some((t, n - 1));
            }
        }
    }

    fn go(&mut self, to: String) {
        let late = self.late.get(&self.current).copied().unwrap_or(0);
        if late > 0 {
            self.pending = Some((to, late));
        } else {
            self.current = to;
            self.arrive();
        }
    }

    fn press(&mut self, node: &Node, long: bool) -> Result<()> {
        self.acts.push(if long {
            Act::LongPress(label(node))
        } else {
            Act::Tap(label(node))
        });
        if node.bounds().is_empty() {
            bail!("the node has empty bounds, nothing to tap");
        }
        let left = self.ignore.entry(self.current.clone()).or_insert(0);
        if *left > 0 {
            *left -= 1;
            return Ok(());
        }
        let screen = self.cur()?;
        let table = if long {
            &screen.long_presses
        } else {
            &screen.taps
        };
        if let Some((_, to)) = table.iter().find(|(s, _)| s.matches(node)) {
            let to = to.clone();
            self.go(to);
        }
        Ok(())
    }

    fn switch_for_start(&mut self, what: &str) {
        if let Some((_, to)) = self.starts.iter().find(|(p, _)| p == what) {
            self.current = to.clone();
            self.pending = None;
        }
    }
}

impl Phone for FakePhone {
    fn connect(&mut self) -> Result<()> {
        self.acts.push(Act::Connect);
        if self.connect_fails {
            bail!("the fake phone refuses to connect");
        }
        Ok(())
    }
    fn wake(&mut self) -> Result<()> {
        self.acts.push(Act::Wake);
        Ok(())
    }
    fn dump(&mut self) -> Result<UiTree> {
        self.acts.push(Act::Dump);
        self.settle();
        let s = self.cur()?;
        if s.unreadable {
            bail!("the fake screen cannot be dumped");
        }
        Ok(UiTree::parse(&s.xml())?)
    }
    fn tap(&mut self, node: &Node) -> Result<()> {
        self.press(node, false)
    }
    fn long_press(&mut self, node: &Node) -> Result<()> {
        self.press(node, true)
    }
    fn swipe(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, ms: u32) -> Result<()> {
        self.acts.push(Act::Swipe(x1, y1, x2, y2, ms));
        if y2 < y1
            && let Some(to) = self.cur()?.swipe_up_to.clone()
        {
            self.go(to);
        }
        Ok(())
    }
    fn back(&mut self) -> Result<()> {
        self.acts.push(Act::Back);
        if let Some(to) = self.cur()?.back_to.clone() {
            self.go(to);
        }
        Ok(())
    }
    fn key(&mut self, code: &str) -> Result<()> {
        self.acts.push(Act::Key(code.into()));
        if let Some((_, to)) = self.cur()?.keys.iter().find(|(c, _)| c == code) {
            let to = to.clone();
            self.go(to);
        }
        Ok(())
    }
    fn start_app(&mut self, package: &str) -> Result<()> {
        self.acts.push(Act::StartApp(package.into()));
        self.switch_for_start(package);
        Ok(())
    }
    fn start_activity(&mut self, component: &str) -> Result<()> {
        self.acts.push(Act::StartActivity(component.into()));
        self.switch_for_start(component);
        Ok(())
    }
    fn top_activity(&mut self) -> Result<String> {
        self.acts.push(Act::TopActivity);
        self.settle();
        Ok(self.cur()?.activity.clone())
    }
    fn screenshot(&mut self, path: &Path) -> Result<()> {
        self.acts.push(Act::Screenshot);
        std::fs::write(path, b"\x89PNG fake")?;
        Ok(())
    }
    fn notifications(&mut self) -> Result<UiTree> {
        self.acts.push(Act::Notifications);
        if self.shade_unreadable {
            bail!("the fake shade cannot be dumped");
        }
        let nodes = self.shade.clone().unwrap_or_default();
        Ok(UiTree::parse(&xml_of(&nodes))?)
    }
    fn now(&self) -> Duration {
        self.clock
    }
    fn sleep(&mut self, d: Duration) {
        self.clock += d;
    }
    fn shell_out(&mut self, tokens: &[&str]) -> Result<String> {
        self.acts
            .push(Act::Shell(tokens.first().copied().unwrap_or("").into()));
        let line = tokens.join(" ");
        match self.shell.as_mut() {
            Some(h) => h(false, &line),
            None => bail!("the fake phone has no shell"),
        }
    }
    fn root(&mut self, script: &super::RootScript) -> Result<String> {
        let line = script.line();
        self.acts.push(Act::Root(
            line.split_whitespace().next().unwrap_or("").into(),
        ));
        match self.shell.as_mut() {
            Some(h) => h(true, &line),
            None => bail!("the fake phone has no shell"),
        }
    }
    fn type_text(&mut self, text: &str) -> Result<()> {
        self.acts.push(Act::TypeText);
        self.typed.push(text.into());
        if let Some(to) = self.cur()?.type_to.clone() {
            self.go(to);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phone() -> FakePhone {
        FakePhone::new("home")
            .screen(
                FakeScreen::new("home")
                    .activity("fake.app/.Home")
                    .node(
                        NodeSpec::new()
                            .id("a:id/open")
                            .text("Open <it> & \"go\"")
                            .clickable(),
                    )
                    .node(
                        NodeSpec::new()
                            .id("a:id/row")
                            .text("Row")
                            .child(NodeSpec::new().id("a:id/inner")),
                    )
                    .on_tap_id("a:id/open", "detail")
                    .on_long_press_text("Row", "menu")
                    .on_tap_text("Open <it> & \"go\"", "detail"),
            )
            .screen(
                FakeScreen::new("detail")
                    .on_back("home")
                    .node(NodeSpec::new().id("a:id/detail")),
            )
            .screen(FakeScreen::new("menu").node(NodeSpec::new().id("a:id/menu")))
            .screen(FakeScreen::unreadable("locked"))
            .screen(FakeScreen::blank("empty"))
            .on_start("other.app", "locked")
    }

    fn node(p: &mut FakePhone, id: &str) -> Node {
        p.dump().unwrap().require_id(id).unwrap().clone()
    }

    #[test]
    fn test_phone_fake_builder_round_trips_through_the_parser() {
        let mut p = phone();
        let t = p.dump().unwrap();
        assert_eq!(t.len(), 3);
        assert!(t.has_text("Open <it> & \"go\""));
        let row = t.require_id("a:id/row").unwrap();
        assert_eq!(t.children(row).len(), 1);
        assert!(t.require_id("a:id/open").unwrap().clickable());
    }

    #[test]
    fn test_phone_fake_transitions_tap_long_press_back() {
        let mut p = phone();
        let open = node(&mut p, "a:id/open");
        p.tap(&open).unwrap();
        assert_eq!(p.current(), "detail");
        p.back().unwrap();
        assert_eq!(p.current(), "home");
        let row = node(&mut p, "a:id/row");
        p.tap(&row).unwrap();
        assert_eq!(p.current(), "home", "a tap with no rule changes nothing");
        p.long_press(&row).unwrap();
        assert_eq!(p.current(), "menu");
        p.back().unwrap();
        assert_eq!(p.current(), "menu", "no back rule: stays");
        assert_eq!(p.top_activity().unwrap(), "fake.app/.Main");
    }

    #[test]
    fn test_phone_fake_logs_acts() {
        let mut p = phone();
        p.connect().unwrap();
        p.wake().unwrap();
        let open = node(&mut p, "a:id/open");
        p.tap(&open).unwrap();
        p.key("KEYCODE_ENTER").unwrap();
        p.swipe(1, 2, 3, 4, 5).unwrap();
        p.start_app("zzz").unwrap();
        assert_eq!(
            p.acts(),
            &[
                Act::Connect,
                Act::Wake,
                Act::Dump,
                Act::Tap("a:id/open".into()),
                Act::Key("KEYCODE_ENTER".into()),
                Act::Swipe(1, 2, 3, 4, 5),
                Act::StartApp("zzz".into()),
            ]
        );
        assert_eq!(p.current(), "detail", "an unknown start changes nothing");
        p.start_app("other.app").unwrap();
        assert_eq!(p.current(), "locked");
    }

    #[test]
    fn test_phone_fake_unreadable_screen_fails_the_dump() {
        let mut p = phone();
        p.set_screen("locked");
        assert!(p.dump().is_err());
        p.set_screen("nowhere");
        assert!(p.dump().is_err());
    }

    #[test]
    fn test_phone_fake_missing_node_screen() {
        let mut p = phone();
        p.set_screen("empty");
        let t = p.dump().unwrap();
        assert!(t.is_empty());
        assert!(t.require_id("a:id/open").is_err());
    }

    #[test]
    fn test_phone_fake_ignored_taps_then_it_works() {
        let mut p = phone();
        p.ignore_taps("home", 2);
        let open = node(&mut p, "a:id/open");
        p.tap(&open).unwrap();
        p.tap(&open).unwrap();
        assert_eq!(p.current(), "home");
        p.tap(&open).unwrap();
        assert_eq!(p.current(), "detail");
    }

    #[test]
    fn test_phone_fake_late_landing() {
        let mut p = phone();
        p.land_late("home", 2);
        let open = node(&mut p, "a:id/open");
        p.tap(&open).unwrap();
        assert_eq!(p.current(), "home");
        assert!(p.dump().unwrap().has_id("a:id/open"));
        assert!(p.dump().unwrap().has_id("a:id/open"));
        assert!(p.dump().unwrap().has_id("a:id/detail"));
    }

    #[test]
    fn test_phone_fake_from_xml_and_shade_and_connect_and_screenshot() {
        let xml = "<hierarchy><node text=\"hi\" resource-id=\"x:id/y\" bounds=\"[0,0][5,5]\"/></hierarchy>";
        let mut p = FakePhone::new("s")
            .screen(FakeScreen::from_xml("s", xml))
            .with_shade(vec![NodeSpec::new().id("n:id/title").text("New message")])
            .failing_connect();
        assert!(p.dump().unwrap().has_id("x:id/y"));
        assert!(p.notifications().unwrap().has_text("New message"));
        assert!(p.connect().is_err());
        let f = std::env::temp_dir().join(format!("tp-fake-{}.png", std::process::id()));
        p.screenshot(&f).unwrap();
        assert!(f.exists());
        let _ = std::fs::remove_file(&f);
        let mut q = FakePhone::new("s")
            .screen(FakeScreen::blank("s"))
            .with_unreadable_shade();
        assert!(q.notifications().is_err());
        assert!(q.dump().unwrap().is_empty());
    }

    #[test]
    fn test_phone_fake_virtual_clock() {
        let mut p = phone();
        p.sleep(Duration::from_millis(1500));
        assert_eq!(p.now(), Duration::from_millis(1500));
    }

    #[test]
    fn test_phone_fake_tap_of_an_empty_bounds_node_is_refused() {
        let mut p = phone();
        let t = UiTree::parse("<hierarchy><node bounds=\"[0,0][0,0]\"/></hierarchy>").unwrap();
        let n = t.iter().next().unwrap().clone();
        assert!(p.tap(&n).is_err());
    }
}
