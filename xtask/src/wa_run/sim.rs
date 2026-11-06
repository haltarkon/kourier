//! A stateful stand-in for the TEST phone's WhatsApp, shaped like the screens of the survey
//! (`target/wa/phone/ui-survey.md`): the chat list, the selection toolbar, the mute dialog, the
//! Archived list, a group's info, Settings > Chats, the Calls tab and an incoming call. It is a
//! [`Phone`] like `FakePhone`, but it keeps the chats' state (archived, pinned, muted) the way the app
//! does, so the acts the tool makes change what the next screen shows. The rehearsal's steps run
//! against it (`--test-phone fake:rehearsal`, in a rehearsal's place only) and the unit tests of
//! [`super::phone`] use it with its failure knobs (an unreadable screen, an ignored tap, the
//! registration block screen, an act that does not take effect). All names are synthetic.
//!
//! Time is virtual: `sleep` moves the clock, so a hold of 15 s costs nothing.

#![allow(dead_code)]

use crate::test_phone::{Node, Phone, UiTree};
use anyhow::{Result, bail};
use std::time::Duration;

const ID: &str = "com.whatsapp:id/";

thread_local! {
    /// The rehearsal's step says "the personal phone calls now": the simulated phone starts to ring.
    static RING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The rehearsal's personal phone calls the test number now (the step that asked the user to call says so).
pub fn ring_now() {
    RING.with(|r| r.set(true));
}

/// One chat of the simulated list.
#[derive(Clone, Debug)]
pub struct SimChat {
    /// Stands for the name: the row says `Sim Chat <token>`.
    pub token: u32,
    pub group: bool,
    pub pinned: bool,
    pub muted: bool,
    pub archived: bool,
}

impl SimChat {
    pub fn direct(token: u32) -> Self {
        Self {
            token,
            group: false,
            pinned: false,
            muted: false,
            archived: false,
        }
    }
    pub fn group(token: u32) -> Self {
        Self {
            group: true,
            ..Self::direct(token)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Screen {
    Home,
    Selected { token: u32, from_archived: bool },
    Overflow { token: u32, from_archived: bool },
    MuteDialog { token: u32, from_archived: bool },
    Archived,
    GroupInfo { token: u32 },
    HomeMenu,
    Settings,
    SettingsChat,
    Calls,
    Block,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CallState {
    Idle,
    Ringing,
    Connected,
}

/// The knobs that make the simulation misbehave.
#[derive(Clone, Debug, Default)]
pub struct SimFaults {
    /// Every `dump` fails (uiautomator could not read the screen).
    pub unreadable: bool,
    /// The next n taps and long presses are accepted and do nothing.
    pub ignore_presses: u32,
    /// A tap on a toolbar button of the selection changes nothing (the act does not take effect).
    pub dead_toolbar: bool,
    /// A toolbar button closes the selection and changes nothing (the tap "worked", the act did not).
    pub phantom_acts: bool,
    /// A long press selects the row BELOW the one pressed (the list moved under the finger).
    pub wrong_row_press: bool,
    /// The selected row carries no selected mark (the real hierarchy may not expose one).
    pub no_selected_flag: bool,
    /// The caller on the call screen is not the 1:1 chat.
    pub stranger_calls: bool,
    /// The End control does nothing (the call stays open).
    pub end_does_nothing: bool,
    /// The top activity is WhatsApp's registration block screen from the start.
    pub blocked: bool,
    /// The call screen has no Answer button (only the swipe up answers).
    pub swipe_to_answer: bool,
    /// An ended call is logged as missed.
    pub log_missed: bool,
    /// No call screen at all, whatever rings.
    pub never_rings: bool,
    /// The member list of a group's info ends early (the participants title says more).
    pub short_member_list: bool,
}

pub struct WaSim {
    pub chats: Vec<SimChat>,
    screen: Screen,
    clock: Duration,
    /// The virtual time at which an incoming call starts to ring (None: no call).
    ring_at: Option<Duration>,
    /// How long a call rings before the caller gives up (it is then logged as missed); None: for ever.
    ring_len: Option<Duration>,
    ring_since: Duration,
    call: CallState,
    /// Calls logged on the Calls tab, newest first: true = missed.
    log: Vec<bool>,
    keep_archived: bool,
    /// (token of the member, admin) of the group's info; the first row is "You".
    members: Vec<(u32, bool)>,
    scroll: usize,
    pub faults: SimFaults,
    pub taps: usize,
    pub acts: Vec<String>,
}

impl WaSim {
    /// A group and a 1:1 chat, the member list of the group (You, an admin, one more).
    pub fn standard() -> Self {
        Self {
            chats: vec![SimChat::direct(11), SimChat::group(12), SimChat::direct(13)],
            screen: Screen::Home,
            clock: Duration::ZERO,
            ring_at: None,
            ring_len: None,
            ring_since: Duration::ZERO,
            call: CallState::Idle,
            log: Vec::new(),
            keep_archived: false,
            members: vec![(100, true), (101, false), (102, true)],
            scroll: 0,
            faults: SimFaults::default(),
            taps: 0,
            acts: Vec::new(),
        }
    }
    pub fn ring_len(mut self, len: Duration) -> Self {
        self.ring_len = Some(len);
        self
    }
    pub fn ring_at(mut self, at: Duration) -> Self {
        self.ring_at = Some(at);
        self
    }
    pub fn keep_archived(mut self, on: bool) -> Self {
        self.keep_archived = on;
        self
    }
    pub fn faults(mut self, f: SimFaults) -> Self {
        self.faults = f;
        self
    }
    pub fn with_members(mut self, members: Vec<(u32, bool)>) -> Self {
        self.members = members;
        self
    }
    pub fn chat(&self, token: u32) -> &SimChat {
        self.chats.iter().find(|c| c.token == token).expect("chat")
    }
    pub fn calls_logged(&self) -> usize {
        self.log.len()
    }
    pub fn in_call(&self) -> bool {
        self.call == CallState::Connected
    }
    pub fn is_ringing(&self) -> bool {
        self.call == CallState::Ringing
    }

    fn chat_mut(&mut self, token: u32) -> &mut SimChat {
        self.chats
            .iter_mut()
            .find(|c| c.token == token)
            .expect("chat")
    }

    /// The main list: pinned first, then the rest in their order; archived ones are not in it.
    fn main_list(&self) -> Vec<&SimChat> {
        let (mut pinned, rest): (Vec<_>, Vec<_>) = self
            .chats
            .iter()
            .filter(|c| !c.archived)
            .partition(|c| c.pinned);
        pinned.extend(rest);
        pinned
    }
    fn archived_list(&self) -> Vec<&SimChat> {
        self.chats.iter().filter(|c| c.archived).collect()
    }

    fn tick(&mut self) {
        let asked = RING.with(|r| r.replace(false));
        let due = self.ring_at.is_some_and(|at| self.clock >= at);
        if (asked || due) && self.call == CallState::Idle && !self.faults.never_rings {
            self.call = CallState::Ringing;
            self.ring_since = self.clock;
            self.ring_at = None;
        }
        if self.call == CallState::Ringing
            && self
                .ring_len
                .is_some_and(|len| self.clock >= self.ring_since + len)
        {
            self.call = CallState::Idle;
            self.log.insert(0, true);
        }
    }

    fn name(token: u32) -> String {
        format!("Sim Chat {token}")
    }

    // ---- the screens as XML ----

    fn row_xml(c: &SimChat, y: i32, selected: bool) -> String {
        let from = if c.group {
            format!(
                "<node text=\"Sim Sender\" resource-id=\"{ID}msg_from_tv\" class=\"android.widget.TextView\" package=\"com.whatsapp\" content-desc=\"\" checked=\"false\" clickable=\"false\" enabled=\"true\" bounds=\"[200,{}][400,{}]\"/>",
                y + 60,
                y + 100
            )
        } else {
            String::new()
        };
        format!(
            "<node text=\"\" resource-id=\"{ID}contact_row_container\" class=\"android.widget.LinearLayout\" package=\"com.whatsapp\" content-desc=\"\" checked=\"false\" selected=\"{selected}\" clickable=\"true\" enabled=\"true\" bounds=\"[0,{y}][1080,{}]\">\
             <node text=\"{}\" resource-id=\"{ID}conversations_row_contact_name\" class=\"android.widget.TextView\" package=\"com.whatsapp\" content-desc=\"\" checked=\"false\" clickable=\"false\" enabled=\"true\" bounds=\"[200,{y}][800,{}]\"/>\
             {from}\
             <node text=\"words\" resource-id=\"{ID}single_msg_tv\" class=\"android.widget.TextView\" package=\"com.whatsapp\" content-desc=\"\" checked=\"false\" clickable=\"false\" enabled=\"true\" bounds=\"[400,{}][900,{}]\"/>\
             </node>",
            y + 140,
            Self::name(c.token),
            y + 50,
            y + 60,
            y + 100
        )
    }

    fn n(id: &str, text: &str, desc: &str, clickable: bool, checked: bool, b: [i32; 4]) -> String {
        let id = if id.is_empty() {
            String::new()
        } else if id.contains(':') {
            id.to_owned()
        } else {
            format!("{ID}{id}")
        };
        format!(
            "<node text=\"{text}\" resource-id=\"{id}\" class=\"android.view.View\" package=\"com.whatsapp\" content-desc=\"{desc}\" checked=\"{checked}\" clickable=\"{clickable}\" enabled=\"true\" bounds=\"[{},{}][{},{}]\"/>",
            b[0], b[1], b[2], b[3]
        )
    }

    fn is_selected(&self, token: u32) -> bool {
        !self.faults.no_selected_flag
            && matches!(&self.screen,
                Screen::Selected { token: t, .. } | Screen::Overflow { token: t, .. } | Screen::MuteDialog { token: t, .. } if *t == token)
    }

    fn nav(&self) -> String {
        let selected = if self.screen == Screen::Calls {
            "Calls"
        } else {
            "Chats"
        };
        [
            ("Chats", 135),
            ("Updates", 405),
            ("Communities", 675),
            ("Calls", 945),
        ]
        .iter()
        .map(|(d, x)| {
            let node = Self::n("", "", d, true, false, [x - 100, 1760, x + 100, 1860]);
            if *d == selected {
                // The selected tab shows WhatsApp's large label and, as on the real phone (R1), is NOT
                // clickable but selected.
                let node = node.replace("clickable=\"true\"", "clickable=\"false\" selected=\"true\"");
                format!(
                    "{}><node text=\"{d}\" resource-id=\"{ID}navigation_bar_item_large_label_view\" class=\"android.view.View\" package=\"com.whatsapp\" content-desc=\"\" checked=\"false\" clickable=\"false\" enabled=\"true\" bounds=\"[{},1800][{},1850]\"/></node>",
                    node.trim_end_matches("/>"),
                    x - 50,
                    x + 50
                )
            } else {
                node
            }
        })
        .collect()
    }

    fn toolbar(&self, token: u32, from_archived: bool) -> String {
        let c = self.chat(token);
        let mut s = String::from(&Self::n(
            "action_mode_bar",
            "",
            "",
            false,
            false,
            [0, 75, 1080, 175],
        ));
        let _ = from_archived;
        s.push_str(&Self::n(
            "action_bar_title",
            "1",
            "",
            false,
            false,
            [60, 75, 200, 175],
        ));
        // Under Archived the real toolbar has no pin button (R1).
        if !c.archived {
            s.push_str(&Self::n(
                "menuitem_conversations_pin",
                "",
                if c.pinned { "Unpin chat" } else { "Pin chat" },
                true,
                false,
                [528, 75, 628, 175],
            ));
        }
        s.push_str(&Self::n(
            "menuitem_conversations_delete",
            "",
            "Delete chat",
            true,
            false,
            [644, 75, 744, 175],
        ));
        s.push_str(&Self::n(
            "menuitem_mute",
            "",
            if c.muted {
                "Unmute notifications"
            } else {
                "Mute notifications"
            },
            true,
            false,
            [760, 75, 860, 175],
        ));
        s.push_str(&Self::n(
            // Under Archived the real toggle is `..._unarchive` (R1).
            if c.archived {
                "menuitem_conversations_unarchive"
            } else {
                "menuitem_conversations_archive"
            },
            "",
            if c.archived {
                "Unarchive chat"
            } else {
                "Archive chat"
            },
            true,
            false,
            [876, 75, 976, 175],
        ));
        s.push_str(&Self::n(
            "menuitem_overflow",
            "",
            "More options",
            true,
            false,
            [980, 75, 1080, 175],
        ));
        s
    }

    fn xml(&self) -> String {
        let mut body = String::new();
        let sel_archived = matches!(
            &self.screen,
            Screen::Selected {
                from_archived: true,
                ..
            } | Screen::Overflow {
                from_archived: true,
                ..
            } | Screen::MuteDialog {
                from_archived: true,
                ..
            }
        );
        let body_screen = if sel_archived {
            Screen::Archived
        } else {
            self.screen.clone()
        };
        match &body_screen {
            Screen::Home
            | Screen::Selected { .. }
            | Screen::Overflow { .. }
            | Screen::MuteDialog { .. }
            | Screen::HomeMenu => {
                if matches!(self.screen, Screen::Home | Screen::HomeMenu) {
                    body.push_str(&Self::n(
                        "toolbar",
                        "",
                        "",
                        false,
                        false,
                        [0, 75, 1080, 175],
                    ));
                    body.push_str(&Self::n(
                        "menuitem_overflow",
                        "",
                        "More options",
                        true,
                        false,
                        [980, 75, 1080, 175],
                    ));
                }
                let mut y = 300;
                if !self.archived_list().is_empty() {
                    body.push_str(&format!(
                        "<node text=\"\" resource-id=\"{ID}archived_row\" class=\"android.widget.LinearLayout\" package=\"com.whatsapp\" content-desc=\"\" checked=\"false\" clickable=\"true\" enabled=\"true\" bounds=\"[0,{y}][1080,{}]\">{}{}</node>",
                        y + 140,
                        Self::n("", "Archived", "", false, false, [200, y, 600, y + 50]),
                        Self::n("", &self.archived_list().len().to_string(), "", false, false, [900, y, 1000, y + 50]),
                    ));
                    y += 150;
                }
                for c in self.main_list() {
                    body.push_str(&Self::row_xml(c, y, self.is_selected(c.token)));
                    y += 150;
                }
                body.push_str(&self.nav());
            }
            Screen::Archived => {
                body.push_str(&Self::n(
                    "toolbar",
                    "Archived",
                    "",
                    false,
                    false,
                    [0, 75, 1080, 175],
                ));
                let mut y = 300;
                for c in self.archived_list() {
                    body.push_str(&Self::row_xml(c, y, self.is_selected(c.token)));
                    y += 150;
                }
            }
            Screen::GroupInfo { .. } => {
                let shown = if self.faults.short_member_list {
                    self.members.len().saturating_sub(1)
                } else {
                    self.members.len()
                };
                body.push_str(&Self::n(
                    "participants_title",
                    &format!("{} members", self.members.len()),
                    "",
                    false,
                    false,
                    [0, 300, 600, 360],
                ));
                let mut y = 400;
                for (i, (t, admin)) in self
                    .members
                    .iter()
                    .enumerate()
                    .take(shown)
                    .skip(self.scroll)
                    .take(4)
                {
                    let name = if i == 0 {
                        "You".to_owned()
                    } else {
                        Self::name(*t)
                    };
                    let admin_node = if *admin {
                        Self::n(
                            "owner_view",
                            "Group Admin",
                            "",
                            false,
                            false,
                            [800, y, 1000, y + 50],
                        )
                    } else {
                        String::new()
                    };
                    body.push_str(&format!(
                        "<node text=\"\" resource-id=\"{ID}group_chat_info_layout_root\" class=\"android.widget.LinearLayout\" package=\"com.whatsapp\" content-desc=\"\" checked=\"false\" clickable=\"true\" enabled=\"true\" bounds=\"[0,{y}][1080,{}]\">{}{admin_node}</node>",
                        y + 140,
                        Self::n("name", &name, "", false, false, [200, y, 700, y + 50]),
                    ));
                    y += 150;
                }
            }
            Screen::Settings => {
                body.push_str(&Self::n(
                    "settings_chat",
                    "Chats",
                    "",
                    true,
                    false,
                    [0, 1500, 1080, 1600],
                ));
            }
            Screen::SettingsChat => {
                body.push_str(&format!(
                    "<node text=\"\" resource-id=\"{ID}chat_settings_notify_new_message_switch_view\" class=\"android.widget.LinearLayout\" package=\"com.whatsapp\" content-desc=\"\" checked=\"false\" clickable=\"true\" enabled=\"true\" bounds=\"[0,1800][1080,1920]\">{}{}</node>",
                    Self::n("switch_view_title", "Keep chats archived", "", false, false, [50, 1800, 700, 1850]),
                    Self::n("switch_view_toggle", "", "", true, self.keep_archived, [888, 1823, 1042, 1900]),
                ));
                // The other switch that shares the toggle id: always on.
                body.push_str(&format!(
                    "<node text=\"\" resource-id=\"{ID}other_switch_row\" class=\"android.widget.LinearLayout\" package=\"com.whatsapp\" content-desc=\"\" checked=\"false\" clickable=\"true\" enabled=\"true\" bounds=\"[0,1650][1080,1770]\">{}{}</node>",
                    Self::n("switch_view_title", "Recent media suggestions", "", false, false, [50, 1650, 700, 1700]),
                    Self::n("switch_view_toggle", "", "", true, true, [888, 1673, 1042, 1750]),
                ));
            }
            Screen::Calls => {
                body.push_str(&Self::n(
                    "calls_recyclerView",
                    "",
                    "",
                    false,
                    false,
                    [0, 200, 1080, 1700],
                ));
                let mut y = 400;
                for (i, missed) in self.log.iter().enumerate() {
                    let n = self.log.len() - i;
                    body.push_str(&format!(
                        "<node text=\"\" resource-id=\"{ID}call_row_container\" class=\"android.widget.LinearLayout\" package=\"com.whatsapp\" content-desc=\"\" checked=\"false\" clickable=\"true\" enabled=\"true\" bounds=\"[0,{y}][1080,{}]\">{}{}{}</node>",
                        y + 140,
                        Self::n("contact_name", "Sim Chat 11", "", false, false, [200, y, 700, y + 50]),
                        Self::n(
                            "subtitle",
                            &format!("12:{n:02}"),
                            "",
                            false,
                            false,
                            [200, y + 60, 700, y + 100]
                        ),
                        Self::n(
                            "call_type_icon",
                            "",
                            if *missed { "Missed voice call" } else { "Incoming voice call" },
                            false,
                            false,
                            [100, y, 160, y + 50]
                        ),
                    ));
                    y += 150;
                }
                body.push_str(&self.nav());
            }
            Screen::Block => {
                body.push_str(&Self::n(
                    "custom_registration_block_screen_body",
                    "block",
                    "",
                    false,
                    false,
                    [0, 300, 1080, 600],
                ));
            }
        }
        // overlays
        match &self.screen {
            Screen::Selected {
                token,
                from_archived,
            } => body.push_str(&self.toolbar(*token, *from_archived)),
            Screen::Overflow {
                token,
                from_archived,
            } => {
                body.push_str(&self.toolbar(*token, *from_archived));
                if self.chat(*token).group {
                    body.push_str(&format!(
                        "<node text=\"\" resource-id=\"{ID}menuitem_conversations_group_info\" class=\"android.widget.Button\" package=\"com.whatsapp\" content-desc=\"\" checked=\"false\" clickable=\"true\" enabled=\"true\" bounds=\"[600,250][1080,350]\">{}</node>",
                        Self::n("title", "Group info", "", false, false, [620, 260, 900, 340])
                    ));
                }
                body.push_str(&format!(
                    "<node text=\"\" resource-id=\"{ID}menuitem_conversations_mark_unread\" class=\"android.widget.Button\" package=\"com.whatsapp\" content-desc=\"\" checked=\"false\" clickable=\"true\" enabled=\"true\" bounds=\"[600,360][1080,460]\">{}</node>",
                    Self::n("title", "Mark as unread", "", false, false, [620, 370, 900, 450])
                ));
            }
            Screen::MuteDialog {
                token,
                from_archived,
            } => {
                body.push_str(&self.toolbar(*token, *from_archived));
                body.push_str(&Self::n(
                    "alertTitle",
                    "Mute message notifications",
                    "",
                    false,
                    false,
                    [100, 500, 980, 560],
                ));
                body.push_str(&Self::n(
                    "android:id/button2",
                    "Cancel",
                    "",
                    true,
                    false,
                    [500, 900, 700, 980],
                ));
                body.push_str(&Self::n(
                    "android:id/button1",
                    "OK",
                    "",
                    true,
                    false,
                    [750, 900, 950, 980],
                ));
            }
            Screen::HomeMenu => {
                body.push_str(&format!(
                    "<node text=\"\" resource-id=\"{ID}menuitem_settings\" class=\"android.widget.Button\" package=\"com.whatsapp\" content-desc=\"Settings\" checked=\"false\" clickable=\"true\" enabled=\"true\" bounds=\"[600,800][1080,900]\">{}</node>",
                    Self::n("title", "Settings", "", false, false, [620, 810, 900, 890])
                ));
            }
            _ => {}
        }
        // A call overlays everything.
        match self.call {
            CallState::Ringing => {
                let who = if self.faults.stranger_calls { 99 } else { 11 };
                body.push_str(&Self::n(
                    "voip_caller",
                    &Self::name(who),
                    "",
                    false,
                    false,
                    [100, 400, 900, 480],
                ));
                if !self.faults.swipe_to_answer {
                    body.push_str(&Self::n(
                        "",
                        "Answer",
                        "Answer",
                        true,
                        false,
                        [100, 1500, 500, 1600],
                    ));
                }
                body.push_str(&Self::n(
                    "",
                    "Decline",
                    "Decline",
                    true,
                    false,
                    [600, 1500, 1000, 1600],
                ));
            }
            CallState::Connected => {
                body.push_str(&Self::n(
                    "",
                    "",
                    "End call",
                    true,
                    false,
                    [400, 1500, 700, 1600],
                ));
            }
            CallState::Idle => {}
        }
        format!(
            "<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation=\"0\">{body}</hierarchy>"
        )
    }

    // ---- the touches ----

    fn find_row_token(&self, node: &Node) -> Option<u32> {
        let (_, y) = node.bounds().center();
        let list: Vec<&SimChat> = match self.screen {
            Screen::Archived => self.archived_list(),
            _ => self.main_list(),
        };
        let skip = if matches!(self.screen, Screen::Archived) || self.archived_list().is_empty() {
            0
        } else {
            1
        };
        let idx = (y - 300) / 150 - skip;
        if idx < 0 {
            return None;
        }
        let idx = if self.faults.wrong_row_press {
            idx + 1
        } else {
            idx
        };
        list.get(idx as usize).map(|c| c.token)
    }

    fn press(&mut self, node: &Node, hold_ms: u32) {
        self.taps += 1;
        self.acts
            .push(format!("press {} {}", node.resource_id(), hold_ms));
        if self.faults.ignore_presses > 0 {
            self.faults.ignore_presses -= 1;
            return;
        }
        let id = node
            .resource_id()
            .strip_prefix(ID)
            .unwrap_or(node.resource_id())
            .to_owned();
        let long = hold_ms >= 1800;
        // The call overlay.
        if self.call == CallState::Ringing && !long {
            if node.desc_is("Answer") {
                self.call = CallState::Connected;
                return;
            }
            if node.desc_is("Decline") {
                self.call = CallState::Idle;
                self.log.insert(0, true);
                return;
            }
        }
        if self.call == CallState::Connected
            && node.desc_is("End call")
            && !self.faults.end_does_nothing
        {
            self.call = CallState::Idle;
            self.log.insert(0, self.faults.log_missed);
            return;
        }
        let screen = self.screen.clone();
        match (screen, id.as_str()) {
            (Screen::Home, "contact_row_container") if long => {
                if let Some(token) = self.find_row_token(node) {
                    self.screen = Screen::Selected {
                        token,
                        from_archived: false,
                    };
                }
            }
            (Screen::Home, "contact_row_container") => {
                self.screen = Screen::Block;
            }
            (Screen::Home, "archived_row") => self.screen = Screen::Archived,
            (Screen::Home, "menuitem_overflow") => self.screen = Screen::HomeMenu,
            (Screen::HomeMenu, "menuitem_settings") => self.screen = Screen::Settings,
            (Screen::Settings, "settings_chat") => self.screen = Screen::SettingsChat,
            (Screen::Archived, "contact_row_container") if long => {
                if let Some(token) = self.find_row_token(node) {
                    self.screen = Screen::Selected {
                        token,
                        from_archived: true,
                    };
                }
            }
            (
                Screen::Selected {
                    token,
                    from_archived,
                },
                tool,
            ) => {
                if self.faults.dead_toolbar && tool != "menuitem_overflow" {
                    return;
                }
                let back = if from_archived {
                    Screen::Archived
                } else {
                    Screen::Home
                };
                if self.faults.phantom_acts && tool != "menuitem_overflow" {
                    self.screen = back;
                    return;
                }
                match tool {
                    "menuitem_conversations_pin" => {
                        let c = self.chat_mut(token);
                        c.pinned = !c.pinned;
                        self.screen = back;
                    }
                    "menuitem_mute" => {
                        if self.chat(token).muted {
                            self.chat_mut(token).muted = false;
                            self.screen = back;
                        } else {
                            self.screen = Screen::MuteDialog {
                                token,
                                from_archived,
                            };
                        }
                    }
                    "menuitem_conversations_archive" | "menuitem_conversations_unarchive" => {
                        let c = self.chat_mut(token);
                        c.archived = !c.archived;
                        self.screen = if self.archived_list().is_empty() {
                            Screen::Home
                        } else {
                            back
                        };
                    }
                    "menuitem_overflow" => {
                        self.screen = Screen::Overflow {
                            token,
                            from_archived,
                        }
                    }
                    _ => {}
                }
            }
            (Screen::Overflow { token, .. }, "menuitem_conversations_group_info") => {
                self.scroll = 0;
                self.screen = Screen::GroupInfo { token };
            }
            (
                Screen::MuteDialog {
                    token,
                    from_archived,
                },
                _,
            ) if node.resource_id() == "android:id/button1" => {
                self.chat_mut(token).muted = true;
                self.screen = if from_archived {
                    Screen::Archived
                } else {
                    Screen::Home
                };
            }
            (
                Screen::MuteDialog {
                    token,
                    from_archived,
                },
                _,
            ) if node.resource_id() == "android:id/button2" => {
                self.screen = Screen::Selected {
                    token,
                    from_archived,
                };
            }
            (Screen::SettingsChat, "switch_view_toggle") => {
                // The real toggle would flip; the tool must never press it.
                self.acts.push("TOGGLE PRESSED".into());
                self.keep_archived = !self.keep_archived;
            }
            _ => {
                if node.desc_is("Calls") {
                    self.screen = Screen::Calls;
                } else if node.desc_is("Chats") {
                    self.screen = Screen::Home;
                }
            }
        }
    }
}

impl Phone for WaSim {
    fn connect(&mut self) -> Result<()> {
        Ok(())
    }
    fn wake(&mut self) -> Result<()> {
        Ok(())
    }
    fn dump(&mut self) -> Result<UiTree> {
        self.tick();
        if self.faults.unreadable {
            bail!("the simulated screen cannot be dumped");
        }
        Ok(UiTree::parse(&self.xml())?)
    }
    fn tap(&mut self, node: &Node) -> Result<()> {
        self.press(node, 50);
        Ok(())
    }
    fn long_press(&mut self, node: &Node) -> Result<()> {
        // WhatsApp's chat list ignores a hold of 800 ms (the survey).
        self.press(node, 800);
        Ok(())
    }
    fn long_press_for(&mut self, node: &Node, ms: u32) -> Result<()> {
        self.press(node, ms);
        Ok(())
    }
    fn swipe(&mut self, _x1: i32, y1: i32, _x2: i32, y2: i32, _ms: u32) -> Result<()> {
        self.acts.push("swipe".into());
        if self.call == CallState::Ringing && self.faults.swipe_to_answer && y2 < y1 {
            self.call = CallState::Connected;
            return Ok(());
        }
        if matches!(self.screen, Screen::GroupInfo { .. }) && y2 < y1 {
            self.scroll = (self.scroll + 4).min(self.members.len().saturating_sub(1));
        }
        Ok(())
    }
    fn back(&mut self) -> Result<()> {
        self.acts.push("back".into());
        self.screen = match self.screen.clone() {
            Screen::Selected { from_archived, .. } | Screen::MuteDialog { from_archived, .. } => {
                if from_archived {
                    Screen::Archived
                } else {
                    Screen::Home
                }
            }
            Screen::Overflow {
                token,
                from_archived,
            } => Screen::Selected {
                token,
                from_archived,
            },
            Screen::GroupInfo { .. } | Screen::Archived | Screen::HomeMenu | Screen::Calls => {
                Screen::Home
            }
            Screen::Settings => Screen::HomeMenu,
            Screen::SettingsChat => Screen::Settings,
            other => other,
        };
        Ok(())
    }
    fn key(&mut self, _code: &str) -> Result<()> {
        Ok(())
    }
    fn start_app(&mut self, package: &str) -> Result<()> {
        self.acts.push(format!("start {package}"));
        if package == "com.whatsapp" {
            self.screen = if self.faults.blocked {
                Screen::Block
            } else {
                Screen::Home
            };
        }
        Ok(())
    }
    fn start_activity(&mut self, _component: &str) -> Result<()> {
        Ok(())
    }
    fn top_activity(&mut self) -> Result<String> {
        self.tick();
        if self.faults.blocked {
            return Ok(
                "com.whatsapp/.registration.app.parole.CustomRegistrationBlockActivity".into(),
            );
        }
        if self.call != CallState::Idle {
            return Ok("com.whatsapp/.voipcalling.VoipActivityV2".into());
        }
        Ok(match &self.screen {
            Screen::Block => {
                "com.whatsapp/.registration.app.parole.CustomRegistrationBlockActivity".into()
            }
            Screen::GroupInfo { .. } => "com.whatsapp/.chatinfo.group.GroupChatInfoActivity".into(),
            Screen::SettingsChat => "com.whatsapp/.settings.ui.chats.SettingsChat".into(),
            Screen::Settings => "com.whatsapp/.settings.ui.main.SettingsTabActivity".into(),
            _ => "com.whatsapp/.home.ui.HomeActivity".into(),
        })
    }
    fn screenshot(&mut self, _path: &std::path::Path) -> Result<()> {
        bail!("the simulated phone takes no pictures")
    }
    fn notifications(&mut self) -> Result<UiTree> {
        self.tick();
        Ok(UiTree::parse(
            "<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation=\"0\"></hierarchy>",
        )?)
    }
    fn now(&self) -> Duration {
        self.clock
    }
    fn sleep(&mut self, d: Duration) {
        self.clock += d;
    }
}
