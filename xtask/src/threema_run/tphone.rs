//! The TEST phone's side of the Threema run (the automated path of `docs/threema-run.md` "With a
//! test phone"): every check the steps make on the TEST phone's Threema instead of asking the user,
//! and every act they do there. Generic phone handling is `crate::test_phone`; this file knows
//! Threema's screens (ids from `target/threema/phone/ui-survey.md`, labels from the app's strings).
//!
//! RULES (the lead's design D2, D3, D6):
//! * FAIL CLOSED. An unreadable screen, a missing node, a refused tap or a timeout is never a pass:
//!   a check answers [`Verdict::No`] only for a screen it READ and that says no, and
//!   [`Verdict::Unknown`] (the step fails with "run `step <name>` again", the run is not halted)
//!   for anything it could not read.
//! * Texts, names and IDs the phone shows are read through `Secret` or compared (`text_is`,
//!   `text_contains`) in memory only; no function here returns, prints or logs one. The words of
//!   every [`Unknown`](Verdict::Unknown) and of every act are FIXED sentences.
//! * Nothing here starts the real service; nothing here is used by a test with a real device.
//!
//! The firewall cut ([`cut_threema`]) stands for the runbook's old "flight mode": a root `iptables`
//! DROP on Threema's uid (and `ip6tables`), with a phone-side timer that removes it even if this
//! tool dies. Real flight mode would also cut the Wi-Fi adb link the tool needs.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use super::prompt::{digest_of_id, valid_id, wipe};
use crate::test_phone::{CutTag, Node, Phone, RootScript, Table, UiTree, tap_and_wait, wait_for};

/// Threema's package and its main activity.
pub(super) const PKG: &str = "ch.threema.app";
/// The app's launcher entry (`.home.HomeActivity` is not exported: `am start` refuses it, RT1 2026-11-05);
/// it opens the main screen.
pub(super) const HOME: &str = "ch.threema.app/.activities.MainActivity";
/// The linked-devices screen's activity: the check that the path really led there (the wizard
/// behind `link_device_button` is a QR scanner and is never opened).
const LINKED_ACTIVITY: &str = "LinkedDevicesActivity";
/// How long one screen change may take.
const SCREEN_WAIT: Duration = Duration::from_secs(12);
/// How long the linked-devices list settles before it is counted.
const LIST_SETTLE: Duration = Duration::from_secs(2);

/// The runbook's menu path, as the labels the check walks (the first is an id, not a text).
pub(super) const PATH_SETTINGS: &str = "Settings";
pub(super) const PATH_ROW: &str = "Threema 2.0 for Desktop (Beta)";
/// The Settings entry that holds "Use Threema Push" (Threema for Android 6.6.0 names it so; seen 2026-11-05).
pub(super) const PATH_NOTIFICATIONS: &str = "Sound & Notifications";

/// Texts the tool writes or has the personal phone write are UNIQUE PER STEP ATTEMPT: `kourier-rt1-<slot>-<six random lower-case
/// letters>`, chosen at the step's first call and kept in its carry, so a copy from an earlier
/// attempt can never count ([`marker_for`]). ASCII words `input text` types as they are.
pub(super) fn marker_for(slot: &str, nonce: &str) -> String {
    format!("kourier-rt1-{slot}-{nonce}")
}

/// Whether `text` has the shape of a marker of the step `slot`: nothing else can be a step's own send.
pub(super) fn is_marker_for(slot: &str, text: &str) -> bool {
    text.strip_prefix(&marker_for(slot, ""))
        .is_some_and(|nonce| !nonce.is_empty() && nonce.bytes().all(|b| b.is_ascii_lowercase()))
}

#[cfg(test)]
thread_local! {
    /// Words the next calls of [`random_letters`] on this thread return (a test that must know them).
    pub(super) static TEST_NONCES: std::cell::RefCell<std::collections::VecDeque<String>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

/// Six random lower-case letters (from the system's random source; the clock only if that fails).
pub(super) fn random_letters() -> String {
    #[cfg(test)]
    if let Some(n) = TEST_NONCES.with(|q| q.borrow_mut().pop_front()) {
        return n;
    }
    let mut bytes = [0u8; 6];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_ok();
    if !ok {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((n >> (i * 7)) & 0xff) as u8;
        }
    }
    bytes.iter().map(|b| char::from(b'a' + b % 26)).collect()
}

/// What the app's strings say a warning about a second connection of the ID or a device looks like.
/// Every text is in `target/threema/phone/apk/th-base.strings` (checked by grep, 2026-11-05): "A
/// connection from a different device with the same Threema ID has been detected", "two or more
/// connections from different devices with the same Threema ID", "Removing linked device may have
/// failed", and the link wizard's "Trust New Device" (it must not stand after a link). Looked for,
/// ignoring case, in the texts of the TEST phone's main screen and notification shade. A line about
/// forward security is NOT among them.
pub(super) const WARNING_FRAGMENTS: [&str; 4] = [
    "connection from a different device",
    "connections from different devices",
    "Removing linked device may have failed",
    "Trust New Device",
];

fn rid(name: &str) -> String {
    format!("{PKG}:id/{name}")
}

fn first_by_id(tree: &UiTree, name: &str) -> Option<Node> {
    tree.by_id(&rid(name)).first().map(|n| (*n).clone())
}

fn first_by_text(tree: &UiTree, text: &str) -> Option<Node> {
    tree.by_text(text).first().map(|n| (*n).clone())
}

// ---- What a check answers ------------------------------------------------------------------------------

/// Where an answer of a step came from; the step's record and the report name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    /// The step looked at the TEST phone and read the screen.
    TestPhone,
    /// The step read Kourier's own state (the app's dev control, the contract lines).
    Kourier,
    /// The user typed it with `answer` in his terminal.
    Typed,
    /// The lead passed it with `answer --relayed` after the user's word in chat.
    Relayed,
    /// Nothing was looked at and nobody answered.
    NotLooked,
}

impl Origin {
    /// The words of the record and the report.
    pub(super) fn word(self) -> &'static str {
        match self {
            Self::TestPhone => "checked on the TEST phone",
            Self::Kourier => "checked in Kourier's own state",
            Self::Typed => "typed by the user",
            Self::Relayed => "relayed by the lead",
            Self::NotLooked => "not looked at",
        }
    }

    /// The short key the supervisor keeps with an answer (the tests pin the round trip).
    #[cfg(test)]
    pub(super) fn key(self) -> &'static str {
        match self {
            Self::TestPhone => "phone",
            Self::Kourier => "kourier",
            Self::Typed => "typed",
            Self::Relayed => "relayed",
            Self::NotLooked => "none",
        }
    }

    pub(super) fn from_key(key: &str) -> Self {
        match key {
            "phone" => Self::TestPhone,
            "kourier" => Self::Kourier,
            "relayed" => Self::Relayed,
            "typed" => Self::Typed,
            // An origin the tool does not know is the weakest one, never the strongest.
            _ => Self::NotLooked,
        }
    }
}

/// A check's result. `No` only from a screen that was read and said no.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Yes,
    No,
    /// Could not be read or done: fixed words, no text of the phone.
    Unknown(&'static str),
    /// Nothing the phone showed can answer it (an EMPTY linked-devices list proves nothing): a person
    /// is asked (`ASK-USER` + `answer --relayed`).
    Ask,
}

impl Verdict {
    pub(super) fn of(yes: bool) -> Self {
        if yes { Self::Yes } else { Self::No }
    }
}

// ---- Getting around ----------------------------------------------------------------------------------------

/// Threema's main screen, reached with the launcher (never with back: back from main leaves the
/// app). The phone is woken first.
pub(super) fn home(phone: &mut dyn Phone) -> Result<UiTree> {
    phone.wake()?;
    phone.start_app(PKG)?;
    let nav = rid("bottom_navigation");
    if let Ok(t) = wait_for(phone, SCREEN_WAIT, |t| t.has_id(&nav)) {
        return Ok(t);
    }
    // The app was deeper in its own screens: ask for the main one by name.
    phone.start_activity(HOME)?;
    wait_for(phone, SCREEN_WAIT, |t| t.has_id(&nav))
        .map_err(|_| anyhow!("Threema's main screen did not come up"))
}

/// The text of the "My ID" node, hashed with the allow-list's digest: the ID itself never leaves
/// this function (it is read through `Secret`, upper-cased in a buffer that is wiped, hashed by
/// `digest_of_id`). Fails when the node does not hold an 8-character ID.
pub(super) fn own_id_digest(phone: &mut dyn Phone) -> Result<String> {
    let t = home(phone)?;
    let tab = first_by_id(&t, "my_profile")
        .ok_or_else(|| anyhow!("the My Profile tab is not on the screen"))?;
    drop(t);
    let my_id = rid("my_id");
    let tree = tap_and_wait(phone, &tab, |t| t.has_id(&my_id), SCREEN_WAIT)?;
    let nodes = tree.by_id(&my_id);
    let [node] = nodes.as_slice() else {
        bail!("the My ID node is not there exactly once");
    };
    node.read_secret().with(|bytes| {
        let mut id: Vec<u8> = bytes
            .iter()
            .filter(|b| !b.is_ascii_whitespace())
            .map(u8::to_ascii_uppercase)
            .collect();
        let digest = if valid_id(&id) {
            digest_of_id(&id)
        } else {
            Err(anyhow!("the My ID node does not hold an 8-character ID"))
        };
        wipe(&mut id);
        digest
    })
}

/// The runbook's path to the linked-devices list: the labels found, in order (`overflow`,
/// `Settings`, the row). `Err` when a step of the path cannot be done; the labels so far are lost
/// with it (a path that was not walked to its end is no "yes").
pub(super) struct LinkedDevices {
    pub tree: UiTree,
    pub path: Vec<&'static str>,
}

/// Opens Threema's linked-devices list by the menu path: overflow, Settings, the "Threema 2.0 for
/// Desktop (Beta)" row. The screen is checked to be `LinkedDevicesActivity`; the add-device button is
/// never tapped.
pub(super) fn open_linked_devices(phone: &mut dyn Phone) -> Result<LinkedDevices> {
    let mut path = Vec::new();
    let t = home(phone)?;
    let overflow = first_by_id(&t, "action_overflow_button")
        .ok_or_else(|| anyhow!("the overflow button is not on the screen"))?;
    drop(t);
    let t = tap_and_wait(phone, &overflow, |t| t.has_text(PATH_SETTINGS), SCREEN_WAIT)?;
    path.push("overflow");
    let settings = first_by_text(&t, PATH_SETTINGS)
        .ok_or_else(|| anyhow!("the Settings entry is not on the screen"))?;
    drop(t);
    let title = rid("title_text_view");
    tap_and_wait(phone, &settings, |t| t.has_id(&title), SCREEN_WAIT)?;
    path.push(PATH_SETTINGS);
    // The row may be below the fold: up to five swipes.
    let mut row = None;
    for swipe in 0..=5 {
        let t = phone.dump()?;
        if let Some(n) = first_by_text(&t, PATH_ROW) {
            row = Some(n);
            break;
        }
        drop(t);
        if swipe < 5 {
            phone.swipe(540, 1600, 540, 500, 400)?;
            phone.sleep(Duration::from_millis(400));
        }
    }
    let row = row.ok_or_else(|| anyhow!("the multi-device row is not in Settings"))?;
    let button = rid("link_device_button");
    let tree = tap_and_wait(phone, &row, |t| t.has_id(&button), SCREEN_WAIT)?;
    path.push(PATH_ROW);
    let top = phone.top_activity()?;
    if !top.ends_with(LINKED_ACTIVITY) {
        bail!("the path did not end on the linked-devices screen");
    }
    Ok(LinkedDevices { tree, path })
}

/// What the linked-devices list holds, as counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct DeviceList {
    pub rows: usize,
    /// Rows labelled `Kourier` with this PC's host name.
    pub mine: usize,
    /// Rows labelled `Kourier` (any host), mine included.
    pub kourier: usize,
    /// Rows the phone could not read ("Device" / "Information could not be obtained").
    pub unlabelled: usize,
}

/// Counts the device rows of a linked-devices screen. `host` is compared in memory.
pub(super) fn count_devices(tree: &UiTree, host: &str) -> DeviceList {
    let first = rid("firstLineTv");
    let second = rid("secondLineTv");
    let mut list = DeviceList::default();
    for n in tree.by_id(&first) {
        list.rows += 1;
        let mut unreadable = n.text_is("Device");
        if let Some(card) = tree.parent(n) {
            unreadable |= tree
                .descendants(card)
                .into_iter()
                .any(|d| d.resource_id() == second && d.text_contains("Information could not"));
        }
        if unreadable {
            list.unlabelled += 1;
        }
        if n.text_contains("Kourier") {
            list.kourier += 1;
            if !host.is_empty() && n.text_contains(host) {
                list.mine += 1;
            }
        }
    }
    list
}

/// The list must have been counted equal at least this long after it opened.
const LIST_MIN_LOAD: Duration = Duration::from_secs(6);

/// Opens the list, waits until it has been counted equal twice in a row at least [`LIST_MIN_LOAD`]
/// after it opened, and counts it. Only ROWS prove that a list loaded: an empty count proves nothing
/// (the empty text of the screen is unproven, the loader is an image view), so callers treat zero
/// rows as [`Verdict::Ask`]. The labels of the path come back too. An
/// unreadable or never-loaded list is an error, never a count.
pub(super) fn read_device_list(
    phone: &mut dyn Phone,
    host: &str,
) -> Result<(DeviceList, Vec<&'static str>)> {
    let opened = open_linked_devices(phone)?;
    let path = opened.path;
    drop(opened.tree);
    let started = phone.now();
    let button = rid("link_device_button");
    let mut last: Option<DeviceList> = None;
    for _ in 0..8 {
        phone.sleep(LIST_SETTLE);
        let t = phone.dump()?;
        if !t.has_id(&button) {
            bail!("the linked-devices screen left the screen while it was counted");
        }
        let now = count_devices(&t, host);
        if last == Some(now) && phone.now().saturating_sub(started) >= LIST_MIN_LOAD {
            return Ok((now, path));
        }
        last = Some(now);
    }
    bail!("the linked-devices list was not proved loaded and stable")
}

const LIST_UNREAD: &str = "the TEST phone's linked-devices list could not be read";

/// `list-shows-kourier`: a row labelled Kourier with this host and no unlabelled row.
pub(super) fn list_shows_kourier(
    phone: &mut dyn Phone,
    host: &str,
) -> (Verdict, Option<DeviceList>) {
    match read_device_list(phone, host) {
        Err(_) => (Verdict::Unknown(LIST_UNREAD), None),
        // N1: no rows is no evidence (not loaded, or a failed fetch): a person is asked.
        Ok((list, _)) if list.rows == 0 => (Verdict::Ask, Some(list)),
        Ok((list, _)) => (
            Verdict::of(list.mine >= 1 && list.unlabelled == 0),
            Some(list),
        ),
    }
}

/// `menu-path-as-runbook`: yes when the three labels of the runbook's path exist, in order, on the
/// TEST phone (the path the tool walked, not the one a person used).
pub(super) fn menu_path_as_runbook(phone: &mut dyn Phone) -> Verdict {
    match open_linked_devices(phone) {
        Ok(opened) => Verdict::of(opened.path == ["overflow", PATH_SETTINGS, PATH_ROW]),
        Err(_) => Verdict::Unknown("the runbook's menu path could not be walked on the TEST phone"),
    }
}

/// `tp-list-clean`: no Kourier row and no unlabelled row, on a list proved loaded.
pub(super) fn list_is_clean(phone: &mut dyn Phone, host: &str) -> Verdict {
    match read_device_list(phone, host) {
        Err(_) => Verdict::Unknown(LIST_UNREAD),
        // N1: an EMPTY list is never a checked yes; a list WITH rows is judged by them.
        Ok((list, _)) if list.rows == 0 => Verdict::Ask,
        Ok((list, _)) => Verdict::of(list.kourier == 0 && list.unlabelled == 0),
    }
}

// ---- Warnings, push ----------------------------------------------------------------------------------------

fn holds_a_warning(tree: &UiTree) -> bool {
    tree.iter()
        .any(|n| WARNING_FRAGMENTS.iter().any(|w| n.text_contains_ci(w)))
}

/// The TEST phone's half of `phone-warning`: its main screen and notification shade show none of the
/// app's warning strings. A screen or shade that cannot be read is no "no warning". (The personal
/// phone's half is a question for a person.)
pub(super) fn no_warning_on_test_phone(phone: &mut dyn Phone) -> Verdict {
    let Ok(main) = home(phone) else {
        return Verdict::Unknown("the TEST phone's main screen could not be read");
    };
    let screen_warns = holds_a_warning(&main);
    drop(main);
    let Ok(shade) = phone.notifications() else {
        return Verdict::Unknown("the TEST phone's notification shade could not be read");
    };
    let shade_warns = holds_a_warning(&shade);
    // A warning on either is a yes to "does a phone warn".
    Verdict::of(!(screen_warns || shade_warns))
}

/// `push-own`: Threema's notification setting "Use Threema Push" is switched on. Walks overflow >
/// Settings > Sound & Notifications to the row's clickable container and reads the switch in the row's
/// `android:id/widget_frame` (AndroidX Preference puts it there). UNPROVEN on the real screen; the
/// caller records `Unknown` as "not looked at" and goes on.
pub(super) fn push_is_own(phone: &mut dyn Phone) -> Verdict {
    let mut go = || -> Result<Verdict> {
        let t = home(phone)?;
        let overflow =
            first_by_id(&t, "action_overflow_button").ok_or_else(|| anyhow!("no overflow"))?;
        drop(t);
        let t = tap_and_wait(phone, &overflow, |t| t.has_text(PATH_SETTINGS), SCREEN_WAIT)?;
        let settings = first_by_text(&t, PATH_SETTINGS).ok_or_else(|| anyhow!("no Settings"))?;
        drop(t);
        let title = rid("title_text_view");
        tap_and_wait(phone, &settings, |t| t.has_id(&title), SCREEN_WAIT)?;
        let t = phone.dump()?;
        let notifications = first_by_text(&t, PATH_NOTIFICATIONS)
            .ok_or_else(|| anyhow!("no Notifications entry"))?;
        drop(t);
        let t = tap_and_wait(
            phone,
            &notifications,
            |t| t.has_text("Use Threema Push"),
            SCREEN_WAIT,
        )?;
        let label = t
            .by_text("Use Threema Push")
            .first()
            .map(|n| (*n).clone())
            .ok_or_else(|| anyhow!("no push row"))?;
        let Some(row) = t.clickable_ancestor(&label).map(|r| (*r).clone()) else {
            bail!("the push row has no clickable container");
        };
        let frames: Vec<&Node> = t
            .descendants(&row)
            .into_iter()
            .filter(|d| d.resource_id() == "android:id/widget_frame")
            .collect();
        let [frame] = frames.as_slice() else {
            bail!("the push row has no single widget frame");
        };
        let switches: Vec<&Node> = t
            .descendants(frame)
            .into_iter()
            .filter(|d| d.class().contains("Switch") || d.class().contains("CheckBox"))
            .collect();
        let [switch] = switches.as_slice() else {
            bail!("the push row has no single switch");
        };
        Ok(Verdict::of(switch.checked()))
    };
    go().unwrap_or(Verdict::Unknown(
        "Threema's notification setting could not be read on the TEST phone",
    ))
}

// ---- The screen: lock and wake ------------------------------------------------------------------------------

/// The `mWakefulness=` word of `dumpsys power`: nothing else of its output is kept.
fn wakefulness(phone: &mut dyn Phone) -> Result<String> {
    let out = phone.shell_out(&["dumpsys", "power"])?;
    out.split_whitespace()
        .find_map(|w| w.strip_prefix("mWakefulness="))
        .map(str::to_owned)
        .filter(|w| w.chars().all(|c| c.is_ascii_alphabetic()))
        .ok_or_else(|| anyhow!("dumpsys power names no wakefulness"))
}

/// ACT: puts the TEST phone to sleep (`KEYCODE_SLEEP`) and CHECKS that it is no longer interactive:
/// `Yes` for the wakefulness `Asleep` or `Dozing` (the TEST phone, an Android 11 phone,
/// goes to `Dozing` after the sleep key: the screen is off or shows the ambient clock, the app is behind
/// the lock screen; RT1 2026-11-05); `Dreaming` (a screen saver, the screen on) and `Awake` are no.
pub(super) fn lock_test_phone(phone: &mut dyn Phone) -> Verdict {
    if phone.key("KEYCODE_SLEEP").is_err() {
        return Verdict::Unknown("the TEST phone refused the sleep key");
    }
    phone.sleep(Duration::from_secs(2));
    match wakefulness(phone) {
        Ok(w) => Verdict::of(w == "Asleep" || w == "Dozing"),
        Err(_) => Verdict::Unknown("the TEST phone's screen state could not be read"),
    }
}

// ---- Chats ----------------------------------------------------------------------------------------------------

/// Which chat a step opens on the TEST phone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChatKind {
    /// The 1:1 chat with the personal ID.
    Direct,
    /// The group of both phones.
    Group,
}

struct Row {
    name: Node,
    group: bool,
}

/// The rows of the chat list on the screen. A row has no id (it is Compose): it is the clickable
/// container above a clickable avatar image's wrapper; its name is its first text; a group row holds a
/// node described "Group Chat".
fn chat_rows(tree: &UiTree) -> Vec<Row> {
    let mut rows = Vec::new();
    for avatar in tree
        .iter()
        .filter(|n| n.clickable() && n.class().ends_with("ImageView"))
    {
        // The avatar sits in a wrapper of its own; the row is the clickable container above it
        // (Threema for Android 6.6.0, seen on the TEST phone in RT1).
        let Some(row) = tree.parent(avatar).and_then(|p| tree.clickable_ancestor(p)) else {
            continue;
        };
        let parts = tree.descendants(row);
        let Some(name) = parts
            .iter()
            .find(|n| n.class().ends_with("TextView") && n.has_text())
        else {
            continue;
        };
        let group = parts.iter().any(|n| n.desc_is("Group Chat"));
        rows.push(Row {
            name: (*name).clone(),
            group,
        });
    }
    rows
}

/// A row's name as an in-memory string (never printed or logged): the chat list is scrolled, so rows
/// are told apart across dumps by it.
fn name_of(row: &Row) -> String {
    row.name
        .read_secret()
        .with(|b| String::from_utf8_lossy(b).into_owned())
}

fn scroll_to_top(phone: &mut dyn Phone) -> Result<()> {
    for _ in 0..6 {
        phone.swipe(540, 500, 540, 1600, 300)?;
        phone.sleep(Duration::from_millis(300));
    }
    Ok(())
}

/// The WHOLE chat list, top to bottom: (name in memory, is a group), each chat once. Ends when a
/// scroll shows nothing new. An empty or unreadable list is an error.
fn scan_chat_list(phone: &mut dyn Phone) -> Result<Vec<(String, bool)>> {
    scroll_to_top(phone)?;
    let mut seen: Vec<(String, bool)> = Vec::new();
    for round in 0..16 {
        let t = phone.dump()?;
        let mut added = 0;
        for row in chat_rows(&t) {
            let name = name_of(&row);
            if !seen.iter().any(|(n, _)| *n == name) {
                seen.push((name, row.group));
                added += 1;
            }
        }
        drop(t);
        if round > 0 && added == 0 {
            break;
        }
        phone.swipe(540, 1600, 540, 500, 400)?;
        phone.sleep(Duration::from_millis(400));
    }
    if seen.is_empty() {
        bail!("the chat list shows no chat");
    }
    Ok(seen)
}

/// Opens the chat of `kind` on the TEST phone: wakes it, goes to the Chats tab, scans the WHOLE list
/// and requires exactly ONE row of that kind (none or several is an error: the row is never guessed;
/// Kourier's state carries no chat names, so the group mark is the only difference), then scrolls to
/// it and taps it. The chat screen is recognised by its compose field.
pub(super) fn open_chat(phone: &mut dyn Phone, kind: ChatKind) -> Result<UiTree> {
    let t = home(phone)?;
    let tab = first_by_id(&t, "messages").ok_or_else(|| anyhow!("no Chats tab"))?;
    drop(t);
    tap_and_wait(phone, &tab, |t| !chat_rows(t).is_empty(), SCREEN_WAIT)?;
    let wants_group = kind == ChatKind::Group;
    let all = scan_chat_list(phone)?;
    let mine: Vec<&(String, bool)> = all.iter().filter(|(_, g)| *g == wants_group).collect();
    let [(target, _)] = mine.as_slice() else {
        bail!("the chat list has no single row of that kind");
    };
    scroll_to_top(phone)?;
    let editor = rid("embedded_text_editor");
    for _ in 0..16 {
        let t = phone.dump()?;
        let found = chat_rows(&t)
            .into_iter()
            .find(|r| r.group == wants_group && name_of(r) == *target);
        drop(t);
        if let Some(row) = found {
            return tap_and_wait(phone, &row.name, |t| t.has_id(&editor), SCREEN_WAIT);
        }
        phone.swipe(540, 1600, 540, 500, 400)?;
        phone.sleep(Duration::from_millis(400));
    }
    bail!("the chat's row was not found again while scrolling")
}

/// How many messages of the open chat hold `marker` (the compose field does not count). In memory.
/// CASE-INSENSITIVE and with surrounding whitespace of the marker trimmed: a keyboard capitalises the
/// first letter of what he types (RT1, 2026-11-05).
pub(super) fn count_marker(tree: &UiTree, marker: &str) -> usize {
    let editor = rid("embedded_text_editor");
    let marker = marker.trim();
    tree.iter()
        .filter(|n| n.resource_id() != editor && n.text_contains_ci(marker))
        .count()
}

/// A marker of ANOTHER step of this run, and how often its own step recorded it in a chat.
#[derive(Clone, Debug)]
pub(super) struct OtherMarker {
    pub step: String,
    pub marker: String,
    pub seen: usize,
}

/// The verdict of [`chat_shows_once_among`] when the step's own marker is missing and a marker of
/// another step shows more often than it was recorded: nothing is decided, nothing halts.
pub(super) const MIXUP: &str = "not decided: a marker of another step was sent (a mix-up)";

/// [`chat_shows_once`]'s verdict, and, for a mix-up, which of the other markers showed extra
/// (its index in the list given) and how many times it shows now.
pub(super) struct Shown {
    pub verdict: Verdict,
    pub mixup: Option<(usize, usize)>,
}

/// How long a text may take to show in a chat that was just opened (the phone may be catching up
/// after a sleep or a cut).
const MESSAGE_WAIT: Duration = Duration::from_secs(60);

/// ACT + CHECK for `tp-shows-once` / `tp-shows-later` / `flight-off-both`: wakes the TEST phone,
/// opens the chat of `kind` and counts `marker` (UNIQUE to this step attempt, so an older copy can
/// never count): waits for it to show (up to a minute), looks again a moment later for a double.
/// `Yes` for exactly one; `No` for none after the wait, or two.
#[cfg(test)]
pub(super) fn chat_shows_once(phone: &mut dyn Phone, kind: ChatKind, marker: &str) -> Verdict {
    chat_shows_once_among(phone, kind, marker, &[]).verdict
}

/// [`chat_shows_once`] that also looks for a MIX-UP: the step's own marker is not there, but another
/// marker of the run (`others`) shows more often than its own step recorded. That is not `No` (a lost
/// text): he sent the wrong text, so nothing is decided and the step is run again.
pub(super) fn chat_shows_once_among(
    phone: &mut dyn Phone,
    kind: ChatKind,
    marker: &str,
    others: &[OtherMarker],
) -> Shown {
    let plain = |verdict| Shown {
        verdict,
        mixup: None,
    };
    let Ok(tree) = open_chat(phone, kind) else {
        return plain(Verdict::Unknown(
            "the chat could not be opened on the TEST phone",
        ));
    };
    let mut n = count_marker(&tree, marker);
    drop(tree);
    if n == 0 {
        match wait_for(phone, MESSAGE_WAIT, |t| count_marker(t, marker) >= 1) {
            Ok(t) => n = count_marker(&t, marker),
            // The chat stayed readable and the text never showed, or the screen stopped dumping.
            Err(_) => {
                return match phone.dump() {
                    Ok(t) => {
                        let extra = others.iter().enumerate().find_map(|(i, o)| {
                            let now = count_marker(&t, &o.marker);
                            (now > o.seen).then_some((i, now))
                        });
                        match extra {
                            Some(found) => Shown {
                                verdict: Verdict::Unknown(MIXUP),
                                mixup: Some(found),
                            },
                            None => plain(Verdict::No),
                        }
                    }
                    Err(_) => plain(Verdict::Unknown("the TEST phone's chat could not be read")),
                };
            }
        }
    }
    if n != 1 {
        return plain(Verdict::No);
    }
    // A duplicate would follow at once.
    phone.sleep(Duration::from_secs(3));
    plain(match phone.dump() {
        Ok(t) => Verdict::of(count_marker(&t, marker) == 1),
        Err(_) => Verdict::Unknown("the TEST phone's chat could not be read"),
    })
}

/// ACT for `mirror`, part 1: opens the 1:1 chat, clears a draft the chat may keep of an earlier
/// attempt (Threema keeps one per chat), types `text` and waits for the send control. NOTHING is sent
/// here: the caller records "send tapped" in the step's carry BEFORE [`mirror_tap`], so a re-run never
/// sends twice.
pub(super) fn mirror_prepare(phone: &mut dyn Phone, text: &str) -> Result<Node> {
    let tree = open_chat(phone, ChatKind::Direct)?;
    if count_marker(&tree, text) >= 1 {
        bail!("the text already stands in the chat: nothing is typed");
    }
    let editor =
        first_by_id(&tree, "embedded_text_editor").ok_or_else(|| anyhow!("no compose field"))?;
    drop(tree);
    phone.tap(&editor)?;
    // A draft (an earlier attempt's, or his own): the end key and deletes, round after round, until a
    // round changes nothing. That the field is EMPTY is then proved after typing: it must hold
    // exactly the text, or nothing is sent.
    let mut previous: Option<Vec<u8>> = None;
    let mut settled = false;
    for _ in 0..8 {
        let t = phone.dump()?;
        let now = first_by_id(&t, "embedded_text_editor")
            .ok_or_else(|| anyhow!("no compose field"))?
            .read_secret()
            .with(<[u8]>::to_vec);
        drop(t);
        if previous.as_ref() == Some(&now) {
            settled = true;
            break;
        }
        previous = Some(now);
        phone.key("KEYCODE_MOVE_END")?;
        for _ in 0..48 {
            phone.key("KEYCODE_DEL")?;
        }
    }
    if !settled {
        bail!("the compose field still changes: it is not provably empty");
    }
    phone.type_text(text)?;
    let send = rid("send_button");
    let tree = wait_for(phone, SCREEN_WAIT, |t| t.has_id(&send))?;
    // N4: the field holds exactly the unique text (nothing of a draft in front of it).
    let exact = first_by_id(&tree, "embedded_text_editor").is_some_and(|e| e.text_is(text));
    if !exact {
        bail!("the compose field does not hold exactly the text: nothing is sent");
    }
    first_by_id(&tree, "send_button").ok_or_else(|| anyhow!("no send control"))
}

/// ACT for `mirror`, part 2: ONE tap on the send control (never a retry: a tap that took effect and
/// a second one would be two messages). Returns when the tap was made; [`chat_shows_once`] judges.
pub(super) fn mirror_tap(phone: &mut dyn Phone, send: &Node) -> Result<()> {
    phone.tap(send)
}

// ---- The firewall cut (the runbook's "flight mode") ------------------------------------------------------------

/// The uid of Threema on the TEST phone, from `dumpsys package`: the first `userId=<digits>`.
pub(super) fn threema_uid(phone: &mut dyn Phone) -> Result<u32> {
    let out = phone.shell_out(&["dumpsys", "package", PKG])?;
    out.split_whitespace()
        .find_map(|w| w.strip_prefix("userId="))
        .and_then(|d| d.parse::<u32>().ok())
        .filter(|uid| (10_000..100_000).contains(uid))
        .ok_or_else(|| anyhow!("no user id for Threema"))
}

/// The nonces of the cuts whose DROP rules for `uid` stand in `table` (any tag `kourier-cut-xxxxxx`).
fn tags_in(phone: &mut dyn Phone, table: Table, uid: u32) -> Result<Vec<String>> {
    let out = phone.root(&RootScript::ListOutput(table))?;
    let uid = uid.to_string();
    let mut tags = Vec::new();
    for l in out.lines() {
        let words: Vec<&str> = l.split_whitespace().collect();
        let owned = words
            .windows(2)
            .any(|w| w[0] == "--uid-owner" && w[1] == uid);
        if !(owned && words.windows(2).any(|w| w[0] == "-j" && w[1] == "DROP")) {
            continue;
        }
        for w in words.windows(2).filter(|w| w[0] == "--comment") {
            let tag = w[1].trim_matches('"');
            if let Some(nonce) = tag.strip_prefix("kourier-cut-")
                && CutTag::new(nonce).is_ok()
            {
                tags.push(nonce.to_owned());
            }
        }
    }
    Ok(tags)
}

/// How many DROP rules of the cut `nonce` stand in each table (IPv4, IPv6).
pub(super) fn cut_rules(phone: &mut dyn Phone, uid: u32, nonce: &str) -> Result<(usize, usize)> {
    let count = |tags: Vec<String>| tags.iter().filter(|t| *t == nonce).count();
    Ok((
        count(tags_in(phone, Table::V4, uid)?),
        count(tags_in(phone, Table::V6, uid)?),
    ))
}

/// Removes every rule of the cut `nonce` (every copy, both tables) and checks none is left.
/// `Ok(true)` when one was there.
fn remove_cut(phone: &mut dyn Phone, uid: u32, nonce: &str) -> Result<bool> {
    let tag = CutTag::new(nonce)?;
    let (v4, v6) = cut_rules(phone, uid, nonce)?;
    for (table, n) in [(Table::V4, v4), (Table::V6, v6)] {
        for _ in 0..n.min(8) {
            phone.root(&RootScript::DeleteDrop {
                table,
                uid,
                tag: tag.clone(),
            })?;
        }
    }
    let (v4, v6) = cut_rules(phone, uid, nonce)?;
    if v4 + v6 != 0 {
        bail!("a rule of the cut still stands after its removal");
    }
    Ok(true)
}

/// A cut of Threema's network on the TEST phone: its user id, its tag and when its phone-side timer
/// ends it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Cut {
    pub uid: u32,
    pub nonce: String,
}

/// ACT: cuts Threema off the network on the TEST phone (OUTPUT is dropped: connections stall) and
/// checks that both tagged rules stand. Rules of an older cut of this tool (a lost record) are
/// removed first. The timer, started in the same root shell, deletes only THIS cut's tagged rules
/// after `seconds`.
pub(super) fn cut_threema(phone: &mut dyn Phone, seconds: u32) -> Result<Cut> {
    let uid = threema_uid(phone)?;
    for table in [Table::V4, Table::V6] {
        for old in tags_in(phone, table, uid)? {
            remove_cut(phone, uid, &old)?;
        }
    }
    let nonce = random_letters();
    phone.root(&RootScript::CutOff {
        uid,
        tag: CutTag::new(&nonce)?,
        seconds,
    })?;
    let (v4, v6) = cut_rules(phone, uid, &nonce)?;
    if v4 == 0 || v6 == 0 {
        // Not trusted either way: take back whatever went in.
        let _ = remove_cut(phone, uid, &nonce);
        bail!("the cut's rules do not stand in both tables");
    }
    Ok(Cut { uid, nonce })
}

/// Re-arms a cut that stands: a NEW tagged pair with a fresh timer goes in, the old pair goes out (the
/// old timer then deletes nothing), and both are checked. The new cut is returned.
pub(super) fn rearm_cut(phone: &mut dyn Phone, old: &Cut, seconds: u32) -> Result<Cut> {
    let nonce = random_letters();
    phone.root(&RootScript::CutOff {
        uid: old.uid,
        tag: CutTag::new(&nonce)?,
        seconds,
    })?;
    let (v4, v6) = cut_rules(phone, old.uid, &nonce)?;
    if v4 == 0 || v6 == 0 {
        let _ = remove_cut(phone, old.uid, &nonce);
        bail!("the re-armed cut's rules do not stand in both tables");
    }
    remove_cut(phone, old.uid, &old.nonce)?;
    Ok(Cut {
        uid: old.uid,
        nonce,
    })
}

/// Whether both rules of the cut still stand (a cut the phone's timer ended is gone).
pub(super) fn cut_stands(phone: &mut dyn Phone, cut: &Cut) -> Result<bool> {
    let (v4, v6) = cut_rules(phone, cut.uid, &cut.nonce)?;
    Ok(v4 >= 1 && v6 >= 1)
}

/// ACT: removes the cut's rules and CHECKS that neither table holds one of it. `Ok(true)` when a rule
/// was there (the cut was in place), `Ok(false)` when none was (the phone-side timer had ended it
/// already: the caller fails the step); an error when a rule is still there after the removals.
pub(super) fn uncut_threema(phone: &mut dyn Phone, cut: &Cut) -> Result<bool> {
    let (v4, v6) = cut_rules(phone, cut.uid, &cut.nonce)?;
    let was = v4 + v6 > 0;
    remove_cut(phone, cut.uid, &cut.nonce)?;
    Ok(was)
}

/// The serial of an adb device as the tool takes it: letters, digits and `. : _ -`, not starting with
/// a dash. (A serial is no secret, but it is a value: its shape is checked once, here.)
pub(super) fn check_serial(serial: &str) -> Result<()> {
    if serial.is_empty()
        || serial.starts_with('-')
        || !serial
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '_' | '-'))
    {
        bail!(
            "the test phone's serial has characters an adb serial does not have: nothing was started"
        );
    }
    Ok(())
}

/// The TEST phone of a run: a rehearsal's is ALWAYS the scripted fake (no test, no rehearsal ever
/// runs `adb`); a real run's is the adb phone, connected and woken.
pub(super) fn open_phone(dir: &super::profile::ProfileDir, serial: &str) -> Result<Box<dyn Phone>> {
    if dir.rehearsal {
        return Ok(Box::new(rehearsal_phone()));
    }
    // No test ever opens adb: a test that reaches a real phone here has forgotten its scripted one.
    if cfg!(test) {
        bail!("a test never opens adb");
    }
    check_serial(serial)?;
    let mut phone = crate::test_phone::AdbPhone::new(serial)?;
    phone.connect()?;
    Ok(Box::new(phone))
}

/// The scripted phone a rehearsal's steps use: Threema's screens with the fake's own ID, a Kourier
/// row labelled with this PC's host, no warning, the push switch on.
pub(super) fn rehearsal_phone() -> crate::test_phone::FakePhone {
    super::tphone_fake::ThreemaFake {
        own_id: super::rehearse::constant("OWN_ID").to_owned(),
        devices: vec![(
            format!("Kourier ({})", host_name().unwrap_or_default()),
            "Linux".to_owned(),
        )],
        ..Default::default()
    }
    .build()
}

/// This computer's host name (`gethostname`), the one the app labels its device with.
/// `None` when it cannot be read: no row is then called "mine".
pub(super) fn host_name() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for `buf.len()` bytes; gethostname writes at most that many.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_owned();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod pure {
    use super::*;

    #[test]
    fn origin_words_and_keys_round_trip() {
        for o in [
            Origin::TestPhone,
            Origin::Kourier,
            Origin::Typed,
            Origin::Relayed,
            Origin::NotLooked,
        ] {
            assert_eq!(Origin::from_key(o.key()), o);
            assert!(!o.word().is_empty());
        }
        assert_eq!(Origin::from_key("anything"), Origin::NotLooked);
    }
}
