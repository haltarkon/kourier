//! A scripted TEST phone that plays Threema's screens (the ids of `ui-survey.md`): the rehearsal's
//! phone (`tphone::open_phone` in a rehearsal) and the tests'. Nothing here touches a device.
//! Every value on its screens is synthetic.

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::rc::Rc;

use crate::test_phone::{FakePhone, FakeScreen, NodeSpec};

use super::tphone::{HOME, PATH_NOTIFICATIONS, PATH_ROW, PATH_SETTINGS, PKG};

/// What the fake shows. The defaults are the "everything is as the run wants" phone.
#[derive(Clone, Debug)]
pub(super) struct ThreemaFake {
    /// The text of `my_id`.
    pub own_id: String,
    /// The linked-devices rows: (first line, second line).
    pub devices: Vec<(String, String)>,
    /// The main screen carries a warning text.
    pub warning_on_screen: bool,
    /// The notification shade carries a warning text.
    pub warning_in_shade: bool,
    /// The shade cannot be read.
    pub shade_unreadable: bool,
    /// "Use Threema Push" is switched on.
    pub push_on: bool,
    /// The texts shown in the 1:1 chat (each a message), and in the group.
    pub direct_texts: Vec<String>,
    pub group_texts: Vec<String>,
    /// Two rows that look like the 1:1 chat (an ambiguous list).
    pub two_direct_rows: bool,
    /// The 1:1 chat shows the text typed into it after the send.
    pub send_shows_text: bool,
    /// The chat list screen cannot be read.
    pub chats_unreadable: bool,
    /// The linked-devices list never loads: no rows and no empty-list text (the static button only).
    pub list_unloaded: bool,
    /// The list shows its empty-list text but a progress indicator still runs.
    pub list_loading: bool,
    /// The notification settings screen cannot be read.
    pub notifs_unreadable: bool,
    /// The text the send of the 1:1 chat shows (the step's unique mirror text).
    pub sent_text: String,
    /// A draft stands in the 1:1 chat's compose field and cannot be wiped by deletes.
    pub draft: bool,
    /// A draft stands in the compose field and the delete key wipes it.
    pub draft_wipable: bool,
}

impl Default for ThreemaFake {
    fn default() -> Self {
        Self {
            own_id: "REHOWN01".into(),
            devices: Vec::new(),
            warning_on_screen: false,
            warning_in_shade: false,
            shade_unreadable: false,
            push_on: true,
            direct_texts: Vec::new(),
            group_texts: Vec::new(),
            two_direct_rows: false,
            send_shows_text: true,
            chats_unreadable: false,
            list_unloaded: false,
            list_loading: false,
            draft: false,
            draft_wipable: false,
            sent_text: String::new(),
            notifs_unreadable: false,
        }
    }
}

fn rid(n: &str) -> String {
    format!("{PKG}:id/{n}")
}

fn nav() -> Vec<NodeSpec> {
    vec![
        NodeSpec::new().id(&rid("bottom_navigation")),
        NodeSpec::new().id(&rid("messages")).clickable(),
        NodeSpec::new().id(&rid("contacts")).clickable(),
        NodeSpec::new().id(&rid("my_profile")).clickable(),
        NodeSpec::new()
            .id(&rid("action_overflow_button"))
            .clickable(),
    ]
}

fn with_nav(mut s: FakeScreen) -> FakeScreen {
    for n in nav() {
        s = s.node(n);
    }
    s.on_tap_id(&rid("my_profile"), "profile")
        .on_tap_id(&rid("messages"), "chats")
        .on_tap_id(&rid("action_overflow_button"), "overflow")
}

/// A chat row as Threema for Android 6.6.0 draws it (seen on the TEST phone, RT1 2026-11-05): a
/// clickable `View` holding a plain `View` that wraps the clickable avatar, then the name, the
/// unread badge in a `View`, the snippet and the time, and a `View` described "Group Chat" for a group.
fn row(name: &str, group: bool) -> NodeSpec {
    let mut r = NodeSpec::new()
        .class("android.view.View")
        .clickable()
        .child(
            NodeSpec::new().class("android.view.View").child(
                NodeSpec::new()
                    .class("android.widget.ImageView")
                    .clickable()
                    .desc("avatar"),
            ),
        )
        .child(NodeSpec::new().class("android.widget.TextView").text(name))
        .child(
            NodeSpec::new()
                .class("android.view.View")
                .child(NodeSpec::new().class("android.widget.TextView").text("1")),
        );
    if group {
        r = r.child(
            NodeSpec::new()
                .class("android.view.View")
                .desc("Group Chat"),
        );
    }
    r
}

fn chat(name: &str, texts: &[String], sent: &str, draft: &str) -> FakeScreen {
    let mut s = FakeScreen::new(name)
        .node(
            NodeSpec::new()
                .id(&rid("embedded_text_editor"))
                .text(draft)
                .clickable(),
        )
        .node(NodeSpec::new().id(&rid("send_button")).clickable());
    for t in texts {
        s = s.node(NodeSpec::new().class("android.widget.TextView").text(t));
    }
    if !sent.is_empty() {
        s = s.on_tap_id(&rid("send_button"), sent);
    }
    s
}

impl ThreemaFake {
    /// The phone. `shell` answers `dumpsys` and the root tables; the tests pass their own handler.
    pub(super) fn build(&self) -> FakePhone {
        let f = self;
        let mut warn_main = Vec::new();
        if f.warning_on_screen {
            warn_main.push(
                NodeSpec::new().class("android.widget.TextView").text(
                    "A connection from a different device with the same ID has been detected",
                ),
            );
        }
        let mut home =
            with_nav(FakeScreen::new("home")).activity("ch.threema.app/.home.HomeActivity");
        for w in warn_main {
            home = home.node(w);
        }
        let profile = with_nav(FakeScreen::new("profile"))
            .node(NodeSpec::new().id(&rid("my_id")).text(&f.own_id))
            .activity("ch.threema.app/.home.HomeActivity");
        let overflow = FakeScreen::new("overflow")
            .node(NodeSpec::new().text(PATH_SETTINGS).clickable())
            .on_tap_text(PATH_SETTINGS, "settings");
        let settings = FakeScreen::new("settings")
            .node(
                NodeSpec::new()
                    .id(&rid("title_text_view"))
                    .text(PATH_SETTINGS),
            )
            .node(NodeSpec::new().text(PATH_NOTIFICATIONS).clickable())
            .node(NodeSpec::new().text(PATH_ROW).clickable())
            .on_tap_text(PATH_ROW, "devices")
            .on_tap_text(PATH_NOTIFICATIONS, "notifs");
        let mut devices = FakeScreen::new("devices")
            .activity("ch.threema.app/.multidevice.LinkedDevicesActivity")
            .node(NodeSpec::new().id(&rid("link_device_button")).clickable());
        if f.devices.is_empty() && !f.list_unloaded {
            devices = devices.node(
                NodeSpec::new()
                    .class("android.widget.TextView")
                    .text("Before linking a new device, please download and open the latest beta version of Threema for Desktop."),
            );
        }
        if f.list_loading {
            devices = devices.node(NodeSpec::new().class("android.widget.ProgressBar"));
        }
        for (first, second) in &f.devices {
            devices = devices.node(
                NodeSpec::new()
                    .clickable()
                    .child(NodeSpec::new().id(&rid("firstLineTv")).text(first))
                    .child(NodeSpec::new().id(&rid("secondLineTv")).text(second)),
            );
        }
        let mut push_switch = NodeSpec::new().class("android.widget.Switch").clickable();
        if f.push_on {
            push_switch = push_switch.checked();
        }
        let notifs = if f.notifs_unreadable {
            FakeScreen::unreadable("notifs")
        } else {
            FakeScreen::new("notifs")
        }
        .node(
            NodeSpec::new()
                .clickable()
                .child(NodeSpec::new().text("Use Threema Push"))
                .child(
                    NodeSpec::new()
                        .id("android:id/widget_frame")
                        .child(push_switch),
                ),
        );
        let mut chats = if f.chats_unreadable {
            FakeScreen::unreadable("chats")
        } else {
            with_nav(FakeScreen::new("chats"))
                .node(row("Rehearsal Thpeer", false))
                .node(row("Rehearsal Thgroup", true))
                .on_tap_text("Rehearsal Thpeer", "chat-direct")
                .on_tap_text("Rehearsal Thgroup", "chat-group")
        };
        if f.two_direct_rows && !f.chats_unreadable {
            chats = chats.node(row("Another Thpeer", false));
        }
        let shade: Vec<NodeSpec> = if f.warning_in_shade {
            vec![NodeSpec::new().text("Removing linked device may have failed")]
        } else {
            vec![NodeSpec::new().text("An unrelated notification")]
        };
        let mut p = FakePhone::new("home")
            .screen(home)
            .screen(profile)
            .screen(overflow)
            .screen(settings)
            .screen(devices)
            .screen(notifs)
            .screen(chats)
            .screen({
                let draft_text = if f.draft || f.draft_wipable {
                    "kourier-rt1-mirror-zzzzzz"
                } else {
                    ""
                };
                let mut direct = chat("chat-direct", &f.direct_texts, "", draft_text)
                    .on_type("chat-direct-typed");
                if f.draft_wipable {
                    direct = direct.on_key("KEYCODE_DEL", "chat-direct-clean");
                }
                direct
            })
            .screen(chat("chat-direct-clean", &f.direct_texts, "", "").on_type("chat-direct-typed"))
            .screen({
                let stuck = if f.draft {
                    "kourier-rt1-mirror-zzzzzz"
                } else {
                    ""
                };
                chat(
                    "chat-direct-typed",
                    &f.direct_texts,
                    "chat-direct-sent",
                    &format!("{stuck}{}", f.sent_text),
                )
            })
            .screen(chat("chat-group", &f.group_texts, "", ""))
            .screen({
                let mut texts = f.direct_texts.clone();
                if f.send_shows_text {
                    texts.push(f.sent_text.clone());
                }
                chat("chat-direct-sent", &texts, "", "")
            })
            .on_visit_replace("chat-direct-sent", "chat-direct", "chat-direct-sent")
            .on_start(PKG, "home")
            .on_start(HOME, "home");
        p = if f.shade_unreadable {
            p.with_unreadable_shade()
        } else {
            p.with_shade(shade)
        };
        p
    }
}

/// The scripted power/firewall shell the tests share: the wakefulness word, Threema's uid, and two
/// tables of OUTPUT rules that insert and delete follow, as the device does. Returns the handler and
/// a view of the tables.
#[cfg(test)]
pub(super) type Tables = Rc<RefCell<(Vec<String>, Vec<String>)>>;

#[cfg(test)]
pub(super) fn shell_handler(
    wakefulness: Rc<RefCell<String>>,
    uid: u32,
    tables: Tables,
) -> impl FnMut(bool, &str) -> anyhow::Result<String> + 'static {
    move |root, line| {
        if !root {
            if line.starts_with("dumpsys power") {
                return Ok(format!(
                    "Power Manager State:\n  mWakefulness={}\n",
                    wakefulness.borrow()
                ));
            }
            if line.starts_with("dumpsys package") {
                return Ok(format!("Package [x]\n    userId={uid}\n"));
            }
            anyhow::bail!("unscripted shell line");
        }
        let mut out = String::new();
        for part in line.split(';').map(str::trim) {
            let part = part.split(" >/dev").next().unwrap_or(part);
            if part.starts_with("nohup") {
                break; // the phone-side timer (its quoted script holds `;`): not simulated
            }
            let mut w = part.split_whitespace();
            let table = w.next().unwrap_or_default();
            let rest: Vec<&str> = w.filter(|t| *t != "-w").collect();
            let mut t = tables.borrow_mut();
            let list = match table {
                "iptables" => &mut t.0,
                "ip6tables" => &mut t.1,
                _ => anyhow::bail!("unscripted root line"),
            };
            match rest.as_slice() {
                ["-I", chain, tail @ ..] => list.push(format!("-A {chain} {}", tail.join(" "))),
                ["-D", chain, tail @ ..] => {
                    let want = format!("-A {chain} {}", tail.join(" "));
                    if let Some(i) = list.iter().position(|r| *r == want) {
                        list.remove(i);
                    }
                }
                ["-S", _] => out.push_str(&list.join("\n")),
                _ => anyhow::bail!("unscripted root line"),
            }
        }
        Ok(out)
    }
}
