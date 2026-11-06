//! The TEST phone's WhatsApp, driven by the run tool (the automated path of `wa-run`): the checks that
//! answer the tool's questions (is a chat archived, what does a group's info say, did the phone ring)
//! and the acts it does itself (archive, unarchive, pin, mute, answer and end a call). Everything is a
//! function over `&mut dyn Phone`; the screens are the survey's (`target/wa/phone/ui-survey.md`).
//!
//! RULES. (1) FAIL CLOSED: an unreadable screen, a missing node, a refused tap, a timeout, or a state
//! that is not the expected one after an act is an `Err` (or a plain "no" for a question), never a pass.
//! (2) Every act checks the top activity before and after; WhatsApp's registration block screen stops it
//! with one fixed sentence ([`BLOCKED`]). (3) Nothing a node says (a name, a count's text, a message) is
//! ever printed, logged or put in an error: a chat is known by a hash of its name taken in memory
//! ([`ChatKey`]), counts are parsed inside [`Secret::with`], the errors here are fixed sentences.
//! (4) No row is ever TAPPED (a tap opened a registration block screen once); rows are only long-pressed
//! (about 2000 ms), and the toolbar's buttons are tapped. The delete button and the "Keep chats archived"
//! switch are never touched. (5) Nothing is sent from the phone.

#![allow(dead_code)]

use crate::test_phone::{Node, Phone, Secret, UiTree, tap_and_wait, wait_for};
use anyhow::{Result, anyhow, bail};
use std::collections::HashMap;
use std::time::Duration;

const ID: &str = "com.whatsapp:id/";
const PACKAGE: &str = "com.whatsapp";
/// The hold WhatsApp's chat list needs to start a selection (900 ms did nothing).
const SELECT_HOLD_MS: u32 = 2000;
/// A screen change.
const SHORT: Duration = Duration::from_secs(8);

/// The sentence of the stop at WhatsApp's registration block screen.
pub const BLOCKED: &str =
    "the TEST phone shows WhatsApp's registration block screen: stop and tell the user";

fn id(name: &str) -> String {
    format!("{ID}{name}")
}

/// A chat on the phone, known by a hash of its row's name read in memory: nothing of the name is kept.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ChatKey {
    /// A hash of the row's name, taken in memory.
    name: u64,
    /// Group or 1:1: two chats with one displayed name and different kinds are two chats (review M2).
    group: bool,
}

impl std::fmt::Debug for ChatKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatKey(..)")
    }
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn hash_of(node: &Node) -> u64 {
    node.read_secret().with(fnv)
}

/// Where a chat is on the phone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Where {
    /// In the main chat list.
    Main,
    /// Under Archived.
    Archived,
    /// In neither (both lists were read).
    Nowhere,
}

/// What the selection toolbar says about the selected chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flags {
    pub pinned: bool,
    pub muted: bool,
    pub archived: bool,
}

/// One row of a chat list.
#[derive(Clone)]
pub struct Row {
    pub key: ChatKey,
    pub group: bool,
    /// The note-to-self row ("(You)").
    pub is_you: bool,
    /// The row carries the selected or checked mark.
    selected: bool,
    node: Node,
}

/// Whether the top activity is WhatsApp's, and not its registration block.
pub fn guard(p: &mut dyn Phone) -> Result<()> {
    let top = p
        .top_activity()
        .map_err(|_| anyhow!("the TEST phone's top screen could not be read"))?;
    if top.contains("CustomRegistrationBlockActivity") {
        bail!(BLOCKED);
    }
    if !top.starts_with(&format!("{PACKAGE}/")) {
        bail!("the TEST phone does not show WhatsApp");
    }
    Ok(())
}

fn wait_top(p: &mut dyn Phone, pred: impl Fn(&str) -> bool, timeout: Duration) -> Result<()> {
    let start = p.now();
    loop {
        if let Ok(top) = p.top_activity() {
            if top.contains("CustomRegistrationBlockActivity") {
                bail!(BLOCKED);
            }
            if pred(&top) {
                return Ok(());
            }
        }
        if p.now().saturating_sub(start) >= timeout {
            bail!("the TEST phone did not reach the expected screen in time");
        }
        p.sleep(Duration::from_millis(300));
    }
}

// ---- reading the screens ----

fn tabs(t: &UiTree) -> Vec<&Node> {
    bottom_tab(t, "Chats")
}

/// A tab of the bottom navigation by its word. The SELECTED tab is not clickable on the real phone
/// (WhatsApp 2.26.42.74, seen in R1 2026-11-05: `clickable=false selected=true`), so a tab counts when
/// it is clickable or selected.
fn bottom_tab<'a>(t: &'a UiTree, word: &str) -> Vec<&'a Node> {
    t.find(|n| (n.clickable() || n.selected()) && n.desc_contains(word) && n.bounds().top > 1000)
}

/// The main screen: WhatsApp's bottom navigation is there.
fn on_home(t: &UiTree) -> bool {
    tabs(t).len() == 1 && bottom_tab(t, "Calls").len() == 1
}

/// The Chats tab is the selected one: it carries WhatsApp's large label (the survey) or the selected mark.
fn chats_selected(t: &UiTree) -> bool {
    tabs(t).first().is_some_and(|tab| {
        tab.selected()
            || t.descendants(tab).iter().any(|n| {
                n.resource_id()
                    .ends_with("navigation_bar_item_large_label_view")
            })
    })
}

fn on_archived_screen(t: &UiTree) -> bool {
    !on_home(t) && t.has_text("Archived") && !t.has_id(&id("action_mode_bar"))
}

fn in_selection(t: &UiTree) -> bool {
    t.has_id(&id("action_mode_bar"))
}

/// The chat rows of a list screen. A row whose name cannot be read is an error.
fn rows_of(t: &UiTree) -> Result<Vec<Row>> {
    let mut out = Vec::new();
    for row in t.by_id(&id("contact_row_container")) {
        let below = t.descendants(row);
        let Some(name) = below
            .iter()
            .find(|n| n.resource_id() == id("conversations_row_contact_name"))
        else {
            // The "Archived" entry may be a row of the same kind; it is no chat.
            if below.iter().any(|n| n.text_is("Archived")) {
                continue;
            }
            bail!("a chat row of the TEST phone shows no name");
        };
        let group = below.iter().any(|n| n.resource_id() == id("msg_from_tv"));
        out.push(Row {
            key: ChatKey {
                name: hash_of(name),
                group,
            },
            group,
            is_you: name.text_contains("(You)"),
            selected: row.selected() || row.checked(),
            node: (*row).clone(),
        });
    }
    // Two rows of ONE screen with the same name and kind cannot be told apart: fail closed (review M2).
    for (i, a) in out.iter().enumerate() {
        if out[i + 1..].iter().any(|b| b.key == a.key) {
            bail!(
                "two chats of the TEST phone show the same name and kind: they cannot be told apart"
            );
        }
    }
    Ok(out)
}

/// The "Archived" entry of the main list (a clickable row that holds the text "Archived").
fn archived_entry(t: &UiTree) -> Option<Node> {
    if !on_home(t) {
        return None;
    }
    t.by_text("Archived")
        .into_iter()
        .find_map(|n| t.clickable_ancestor(n).filter(|c| c.bounds().top < 1000))
        .cloned()
}

fn flags_of_toolbar(t: &UiTree) -> Result<Flags> {
    let desc = |suffix: &str| -> Result<&Node> { t.require_id(&id(suffix)) };
    let mute = desc("menuitem_mute")?;
    // Under Archived the toolbar has no pin button and its archive button is `..._unarchive` (WhatsApp
    // 2.26.42.74, seen in R1 2026-11-05): an archived chat is not pinned.
    let arch = desc(archive_button(t)?)?;
    let pinned = match t.by_id(&id("menuitem_conversations_pin")).first() {
        None => false,
        Some(pin) if pin.desc_is("Unpin chat") => true,
        Some(pin) if pin.desc_is("Pin chat") => false,
        Some(_) => bail!("the pin button of the TEST phone says neither pin nor unpin"),
    };
    let muted = if mute.desc_is("Unmute notifications") {
        true
    } else if mute.desc_is("Mute notifications") {
        false
    } else {
        bail!("the mute button of the TEST phone says neither mute nor unmute");
    };
    let archived = if arch.desc_is("Unarchive chat") {
        true
    } else if arch.desc_is("Archive chat") {
        false
    } else {
        bail!("the archive button of the TEST phone says neither archive nor unarchive");
    };
    Ok(Flags {
        pinned,
        muted,
        archived,
    })
}

// ---- moving about ----

/// Back to the main screen with the chat list. WhatsApp is started (not left by `back`); a leftover
/// screen is backed out of, at most four times.
pub fn home(p: &mut dyn Phone) -> Result<UiTree> {
    p.start_app(PACKAGE)
        .map_err(|_| anyhow!("WhatsApp could not be started on the TEST phone"))?;
    // The launch takes a moment: another app (Threema, after RT1) can still be in front right after
    // it, so wait for WhatsApp's own screen (the registration block still stops it, RT1 2026-11-05).
    let own = format!("{PACKAGE}/");
    wait_top(p, |top| top.starts_with(&own), SHORT)?;
    for _ in 0..5 {
        let t = wait_for(p, SHORT, |t| !t.is_empty())
            .map_err(|_| anyhow!("the TEST phone's screen could not be read"))?;
        guard(p)?;
        if on_home(&t) && !in_selection(&t) {
            // Make sure the Chats tab is the shown one (not Calls, Updates or Communities: review M3).
            if t.has_id(&id("calls_recyclerView")) || !chats_selected(&t) {
                let tab = tabs(&t)[0].clone();
                return tap_and_wait(
                    p,
                    &tab,
                    |t| chats_selected(t) && !t.has_id(&id("calls_recyclerView")),
                    SHORT,
                )
                .map_err(|_| anyhow!("the Chats tab did not open on the TEST phone"));
            }
            return Ok(t);
        }
        p.back()?;
        p.sleep(Duration::from_millis(400));
    }
    bail!("the TEST phone did not come back to WhatsApp's main screen")
}

/// Scroll the list to its top (a few swipes down; harmless on a short list).
fn to_top(p: &mut dyn Phone) {
    for _ in 0..3 {
        let _ = p.swipe(540, 600, 540, 1500, 300);
    }
}

/// Every row of the open list screen, scrolling down until nothing new shows.
fn scan(p: &mut dyn Phone) -> Result<Vec<Row>> {
    to_top(p);
    let mut all: Vec<Row> = Vec::new();
    for _ in 0..8 {
        let t = p
            .dump()
            .map_err(|_| anyhow!("the TEST phone's chat list could not be read"))?;
        let fresh: Vec<Row> = rows_of(&t)?
            .into_iter()
            .filter(|r| !all.iter().any(|a| a.key == r.key))
            .collect();
        let none = fresh.is_empty();
        all.extend(fresh);
        if none {
            break;
        }
        let _ = p.swipe(540, 1500, 540, 600, 300);
        p.sleep(Duration::from_millis(300));
    }
    to_top(p);
    Ok(all)
}

/// From the main screen, open Archived and return the rows there (back on the main screen after).
fn archived_rows(p: &mut dyn Phone) -> Result<Vec<Row>> {
    let t = p.dump()?;
    let Some(entry) = archived_entry(&t) else {
        return Ok(Vec::new());
    };
    drop(t);
    tap_and_wait(p, &entry, on_archived_screen, SHORT)
        .map_err(|_| anyhow!("the Archived list did not open on the TEST phone"))?;
    guard(p)?;
    let rows = scan(p)?;
    p.back()?;
    wait_for(p, SHORT, on_home)
        .map_err(|_| anyhow!("the TEST phone did not come back from the Archived list"))?;
    Ok(rows)
}

/// The rows of the main list, in the phone's order (pinned first), without the note-to-self row.
pub fn main_rows(p: &mut dyn Phone) -> Result<Vec<(ChatKey, bool)>> {
    home(p)?;
    Ok(scan(p)?
        .into_iter()
        .filter(|r| !r.is_you)
        .map(|r| (r.key, r.group))
        .collect())
}

/// The rows under Archived, in the phone's order.
pub fn archived_list(p: &mut dyn Phone) -> Result<Vec<(ChatKey, bool)>> {
    home(p)?;
    Ok(archived_rows(p)?
        .into_iter()
        .map(|r| (r.key, r.group))
        .collect())
}

/// Where the chat is: the main list, Archived, or neither. Both lists are read; a list that cannot be
/// read is an error, never "neither".
pub fn locate(p: &mut dyn Phone, key: ChatKey) -> Result<Where> {
    home(p)?;
    if scan(p)?.iter().any(|r| r.key == key) {
        return Ok(Where::Main);
    }
    if archived_rows(p)?.iter().any(|r| r.key == key) {
        return Ok(Where::Archived);
    }
    Ok(Where::Nowhere)
}

/// The key of the `rank`-th chat of `group` (or 1:1) in the main list, or in Archived.
pub fn key_by_rank(p: &mut dyn Phone, group: bool, rank: usize, archived: bool) -> Result<ChatKey> {
    let rows = if archived {
        archived_list(p)?
    } else {
        main_rows(p)?
    };
    rows.into_iter()
        .filter(|(_, g)| *g == group)
        .nth(rank)
        .map(|(k, _)| k)
        .ok_or_else(|| anyhow!("the TEST phone's list has no such chat to pick"))
}

/// The selection toolbar's count says ONE chat is selected.
fn count_is_one(t: &UiTree) -> bool {
    t.by_id(&id("action_bar_title"))
        .first()
        .is_some_and(|n| digits_of(n.read_secret()) == Some(1))
}

/// After a long press: the selected row must be the key's (review H1). A row that carries the selected mark decides;
/// when no row carries one, the toolbar must count ONE and the list must be the very list the press was made on.
/// Otherwise the selection is backed out of and the act fails closed.
fn verify_selection(
    p: &mut dyn Phone,
    selection: UiTree,
    key: ChatKey,
    order: &[ChatKey],
) -> Result<UiTree> {
    let rows = rows_of(&selection)?;
    let flagged: Vec<&Row> = rows.iter().filter(|r| r.selected).collect();
    let ok = match flagged.as_slice() {
        [one] => one.key == key,
        [] => count_is_one(&selection) && rows.iter().map(|r| r.key).collect::<Vec<_>>() == order,
        _ => false,
    };
    if !ok {
        let _ = p.back();
        bail!(
            "the chat that was selected on the TEST phone is not the one asked for: backed out, nothing was done"
        );
    }
    guard(p)?;
    Ok(selection)
}

/// Select the chat: take its row by key from a FRESH dump (never from an earlier scan: the list may have moved),
/// long-press it at once (about 2000 ms), and check that the selected row is the key's. The list is scrolled down
/// when the row is not on the screen.
fn select_key(p: &mut dyn Phone, key: ChatKey) -> Result<UiTree> {
    guard(p)?;
    to_top(p);
    for _ in 0..8 {
        let t = p
            .dump()
            .map_err(|_| anyhow!("the TEST phone's chat list could not be read"))?;
        let rows = rows_of(&t)?;
        let order: Vec<ChatKey> = rows.iter().map(|r| r.key).collect();
        if let Some(row) = rows.iter().find(|r| r.key == key) {
            let node = row.node.clone();
            drop(t);
            for attempt in 0..2 {
                if attempt > 0 {
                    // A second press only while the row still stands where it was.
                    let again = p.dump().map_err(|_| {
                        anyhow!("the screen could not be read before a second press")
                    })?;
                    if in_selection(&again) {
                        return verify_selection(p, again, key, &order);
                    }
                    if !again.still_shows(&node) {
                        bail!("the screen changed after the press: no second press was made");
                    }
                }
                if p.long_press_for(&node, SELECT_HOLD_MS).is_err() {
                    continue;
                }
                if let Ok(sel) = wait_for(p, Duration::from_secs(3), in_selection) {
                    return verify_selection(p, sel, key, &order);
                }
            }
            bail!("the chat on the TEST phone could not be selected");
        }
        drop(t);
        let _ = p.swipe(540, 1500, 540, 600, 300);
        p.sleep(Duration::from_millis(300));
    }
    bail!("the chat's row is not on the TEST phone's screen")
}

/// Find the chat, go to its list, select it. Returns the toolbar's tree and where the chat was.
fn open_toolbar(p: &mut dyn Phone, key: ChatKey) -> Result<(UiTree, Where)> {
    let place = locate(p, key)?;
    home(p)?;
    match place {
        Where::Main => {}
        Where::Archived => {
            let t = p.dump()?;
            let entry = archived_entry(&t)
                .ok_or_else(|| anyhow!("the Archived entry of the TEST phone is gone"))?;
            drop(t);
            tap_and_wait(p, &entry, on_archived_screen, SHORT)
                .map_err(|_| anyhow!("the Archived list did not open on the TEST phone"))?;
            guard(p)?;
        }
        Where::Nowhere => bail!("the chat is in neither list of the TEST phone"),
    }
    Ok((select_key(p, key)?, place))
}

/// Leave the selection (back, at most twice) and return to the main screen.
fn leave(p: &mut dyn Phone) -> Result<()> {
    for _ in 0..2 {
        if let Ok(t) = p.dump()
            && !in_selection(&t)
        {
            break;
        }
        p.back()?;
        p.sleep(Duration::from_millis(300));
    }
    home(p).map(|_| ())
}

/// A toolbar act: tap the button, wait for the selection to close. The tree is the toolbar's.
/// The selection toolbar's archive toggle: `menuitem_conversations_archive` in the main list,
/// `menuitem_conversations_unarchive` under Archived; exactly one of them.
fn archive_button(t: &UiTree) -> Result<&'static str> {
    let main = t.has_id(&id("menuitem_conversations_archive"));
    let archived = t.has_id(&id("menuitem_conversations_unarchive"));
    match (main, archived) {
        (true, false) => Ok("menuitem_conversations_archive"),
        (false, true) => Ok("menuitem_conversations_unarchive"),
        _ => bail!("the TEST phone's toolbar shows no single archive button"),
    }
}

fn press_toolbar(p: &mut dyn Phone, tree: &UiTree, button: &str) -> Result<()> {
    let node = tree.require_id(&id(button))?.clone();
    tap_and_wait(p, &node, |t| !in_selection(t), SHORT)
        .map_err(|_| anyhow!("the act did not take effect on the TEST phone"))?;
    guard(p)
}

// ---- the acts (each checked afterwards on the phone's own lists) ----

/// What the selection toolbar says about the chat.
pub fn flags(p: &mut dyn Phone, key: ChatKey) -> Result<Flags> {
    let (tree, _) = open_toolbar(p, key)?;
    let f = flags_of_toolbar(&tree);
    leave(p)?;
    f
}

pub fn archive(p: &mut dyn Phone, key: ChatKey) -> Result<()> {
    let (tree, place) = open_toolbar(p, key)?;
    if place != Where::Main || flags_of_toolbar(&tree)?.archived {
        let _ = leave(p);
        bail!("the chat is not in the main list of the TEST phone: nothing was archived");
    }
    press_toolbar(p, &tree, "menuitem_conversations_archive")?;
    if locate(p, key)? != Where::Archived {
        bail!("the chat is not under Archived on the TEST phone after the archive");
    }
    Ok(())
}

pub fn unarchive(p: &mut dyn Phone, key: ChatKey) -> Result<()> {
    let (tree, place) = open_toolbar(p, key)?;
    if place != Where::Archived || !flags_of_toolbar(&tree)?.archived {
        let _ = leave(p);
        bail!("the chat is not under Archived on the TEST phone: nothing was unarchived");
    }
    let button = archive_button(&tree)?;
    press_toolbar(p, &tree, button)?;
    if locate(p, key)? != Where::Main {
        bail!("the chat is not in the main list of the TEST phone after the unarchive");
    }
    Ok(())
}

/// Pin (`on`) or unpin the chat, then read the toolbar again: it must say so.
pub fn pin(p: &mut dyn Phone, key: ChatKey, on: bool) -> Result<()> {
    let (tree, _) = open_toolbar(p, key)?;
    if flags_of_toolbar(&tree)?.pinned == on {
        let _ = leave(p);
        bail!("the chat already is as asked on the TEST phone: nothing was pinned or unpinned");
    }
    press_toolbar(p, &tree, "menuitem_conversations_pin")?;
    if flags(p, key)?.pinned != on {
        bail!("the pin is not as asked on the TEST phone after the act");
    }
    Ok(())
}

/// Mute (for the dialog's first choice) or unmute the chat, then read the toolbar again.
pub fn mute(p: &mut dyn Phone, key: ChatKey, on: bool) -> Result<()> {
    let (tree, _) = open_toolbar(p, key)?;
    if flags_of_toolbar(&tree)?.muted == on {
        let _ = leave(p);
        bail!("the chat already is as asked on the TEST phone: nothing was muted or unmuted");
    }
    if on {
        let button = tree.require_id(&id("menuitem_mute"))?.clone();
        let dialog = tap_and_wait(p, &button, |t| t.has_id(&id("alertTitle")), SHORT)
            .map_err(|_| anyhow!("the mute dialog did not open on the TEST phone"))?;
        let ok = dialog.require_id("android:id/button1")?.clone();
        drop(dialog);
        tap_and_wait(
            p,
            &ok,
            |t| !in_selection(t) && !t.has_id(&id("alertTitle")),
            SHORT,
        )
        .map_err(|_| anyhow!("the mute did not take effect on the TEST phone"))?;
        guard(p)?;
    } else {
        press_toolbar(p, &tree, "menuitem_mute")?;
    }
    if flags(p, key)?.muted != on {
        bail!("the mute is not as asked on the TEST phone after the act");
    }
    Ok(())
}

// ---- a group's info ----

fn digits_of(secret: Secret) -> Option<u64> {
    secret.with(|b| {
        let s = std::str::from_utf8(b).ok()?;
        let digits: String = s
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    })
}

/// Open a group's info (long press, overflow, "Group info"), count the members and the admins in
/// memory and come back. `Err` when the list could not be read completely (its count must equal the
/// participants title's) or the chat is no group.
pub fn group_counts(p: &mut dyn Phone, key: ChatKey) -> Result<(u64, u64)> {
    let (tree, place) = open_toolbar(p, key)?;
    if place != Where::Main {
        let _ = leave(p);
        bail!("the group is not in the main list of the TEST phone");
    }
    // Two nodes may carry this id (the home toolbar's and the action bar's): the action bar's is the last (review L4).
    let overflow = tree
        .by_id(&id("menuitem_overflow"))
        .last()
        .map(|n| (*n).clone())
        .ok_or_else(|| anyhow!("the selection has no menu on the TEST phone"))?;
    drop(tree);
    let menu = tap_and_wait(
        p,
        &overflow,
        |t| {
            t.has_id(&id("menuitem_conversations_group_info"))
                || t.has_id(&id("menuitem_conversations_mark_unread"))
        },
        SHORT,
    )
    .map_err(|_| anyhow!("the selection's menu did not open on the TEST phone"))?;
    let Ok(info) = menu
        .require_id(&id("menuitem_conversations_group_info"))
        .cloned()
    else {
        let _ = leave(p);
        bail!("the chat has no group info on the TEST phone: it is no group");
    };
    drop(menu);
    p.tap(&info)?;
    wait_top(p, |t| t.contains("GroupChatInfoActivity"), SHORT)?;
    let counted = read_members(p);
    for _ in 0..4 {
        if wait_for(p, Duration::from_millis(600), |t| {
            on_home(t) && !in_selection(t)
        })
        .is_ok()
        {
            break;
        }
        p.back()?;
        p.sleep(Duration::from_millis(500));
    }
    home(p)?;
    counted
}

fn read_members(p: &mut dyn Phone) -> Result<(u64, u64)> {
    let mut seen: HashMap<u64, bool> = HashMap::new();
    let mut title: Option<u64> = None;
    let mut quiet = 0;
    for _ in 0..24 {
        let t = p
            .dump()
            .map_err(|_| anyhow!("the group's info could not be read on the TEST phone"))?;
        guard(p)?;
        if title.is_none() {
            title = t
                .by_id(&id("participants_title"))
                .first()
                .and_then(|n| digits_of(n.read_secret()));
        }
        let before = seen.len();
        for row in t.by_id(&id("group_chat_info_layout_root")) {
            let below = t.descendants(row);
            let Some(name) = below.iter().find(|n| n.resource_id() == id("name")) else {
                continue;
            };
            let admin = below
                .iter()
                .any(|n| n.resource_id() == id("owner_view") && n.text_is("Group Admin"));
            seen.insert(hash_of(name), admin);
        }
        quiet = if seen.len() == before { quiet + 1 } else { 0 };
        if quiet >= 2 {
            break;
        }
        let _ = p.swipe(540, 1500, 540, 600, 300);
        p.sleep(Duration::from_millis(400));
    }
    let Some(title) = title else {
        bail!("the group's info shows no member count on the TEST phone");
    };
    let members = seen.len() as u64;
    if members != title {
        bail!("the group's member list could not be read completely on the TEST phone");
    }
    Ok((members, seen.values().filter(|a| **a).count() as u64))
}

// ---- Settings > Chats ----

/// Whether "Keep chats archived" is ON. Opens Settings > Chats, scrolls to the row, reads the switch
/// (selected by its row's title: the toggle id is shared with another switch), never touches it.
pub fn keep_chats_archived(p: &mut dyn Phone) -> Result<bool> {
    let t = home(p)?;
    let overflow = t.require_id(&id("menuitem_overflow"))?.clone();
    drop(t);
    let menu = tap_and_wait(p, &overflow, |t| t.has_id(&id("menuitem_settings")), SHORT)
        .map_err(|_| anyhow!("the main menu did not open on the TEST phone"))?;
    let settings = menu.require_id(&id("menuitem_settings"))?.clone();
    drop(menu);
    tap_and_wait(p, &settings, |t| t.has_id(&id("settings_chat")), SHORT)
        .map_err(|_| anyhow!("Settings did not open on the TEST phone"))?;
    let chats = p.dump()?.require_id(&id("settings_chat"))?.clone();
    p.tap(&chats)?;
    wait_top(p, |t| t.ends_with("SettingsChat"), SHORT)?;
    let mut value = None;
    for _ in 0..8 {
        let t = p
            .dump()
            .map_err(|_| anyhow!("the chat settings could not be read on the TEST phone"))?;
        let rows = t.by_id(&id("chat_settings_notify_new_message_switch_view"));
        if let [row] = rows.as_slice() {
            let below = t.descendants(row);
            let titled = below.iter().any(|n| {
                n.resource_id() == id("switch_view_title") && n.text_is("Keep chats archived")
            });
            let toggles: Vec<&&Node> = below
                .iter()
                .filter(|n| n.resource_id() == id("switch_view_toggle"))
                .collect();
            if titled && let [toggle] = toggles.as_slice() {
                value = Some(toggle.checked());
                break;
            }
        }
        let _ = p.swipe(540, 1500, 540, 600, 300);
        p.sleep(Duration::from_millis(400));
    }
    for _ in 0..4 {
        if wait_for(p, Duration::from_millis(600), on_home).is_ok() {
            break;
        }
        p.back()?;
        p.sleep(Duration::from_millis(500));
    }
    guard(p)?;
    value.ok_or_else(|| {
        anyhow!("the \"Keep chats archived\" switch was not found on the TEST phone")
    })
}

// ---- calls ----

const ANSWER_LABELS: [&str; 2] = ["Answer", "Accept call"];
const END_LABELS: [&str; 3] = ["End call", "Hang up", "Leave call"];

fn labelled<'a>(t: &'a UiTree, labels: &[&str]) -> Option<&'a Node> {
    t.find(|n| labels.iter().any(|l| n.desc_is(l) || n.text_is(l)))
        .into_iter()
        .find_map(|n| t.clickable_ancestor(n))
}

/// WhatsApp's calling activity (the survey could not confirm its name; `voip` is the guess). A call-history or
/// call-landing screen is no ring (review L2).
fn call_screen(top: &str) -> bool {
    top.starts_with(&format!("{PACKAGE}/")) && top.to_ascii_lowercase().contains("voip")
}

/// The call cannot be told from the TEST phone's own chat: the caller on the screen is not the run's 1:1 chat. The
/// step records `a call from someone else`.
#[derive(Debug)]
pub struct CallFromSomeoneElse;

impl std::fmt::Display for CallFromSomeoneElse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a call from someone else: it was not answered")
    }
}

impl std::error::Error for CallFromSomeoneElse {}

/// The ringing call screen could not be read before anything was tapped: the caller is not checked and the call is
/// not answered by the tool.
#[derive(Debug)]
pub struct CallScreenUnreadable;

impl std::fmt::Display for CallScreenUnreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the call screen could not be read on the TEST phone: nothing was tapped")
    }
}

impl std::error::Error for CallScreenUnreadable {}

/// Whether the TEST phone's call screen is gone within `timeout` (the top activity is no longer WhatsApp's call
/// activity). A top screen that cannot be read counts as "not yet".
pub fn wait_call_over(p: &mut dyn Phone, timeout: Duration) -> bool {
    let start = p.now();
    loop {
        if let Ok(top) = p.top_activity()
            && !call_screen(&top)
        {
            return true;
        }
        if p.now().saturating_sub(start) >= timeout {
            return false;
        }
        p.sleep(Duration::from_millis(500));
    }
}

/// A call was answered and the step could not confirm that it ended: it may still be open on the TEST phone.
#[derive(Debug)]
pub struct CallMayBeOpen;

impl std::fmt::Display for CallMayBeOpen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a call may still be open on the TEST phone")
    }
}

impl std::error::Error for CallMayBeOpen {}

/// Whether the TEST phone shows an incoming call within `timeout` (the call activity, or an Answer control anywhere on
/// the screen). A timeout or a screen that cannot be read is "no".
pub fn wait_for_ring(p: &mut dyn Phone, timeout: Duration) -> bool {
    let start = p.now();
    loop {
        if let Ok(top) = p.top_activity()
            && call_screen(&top)
        {
            return true;
        }
        if let Ok(t) = p.dump()
            && labelled(&t, &ANSWER_LABELS).is_some()
        {
            return true;
        }
        if p.now().saturating_sub(start) >= timeout {
            return false;
        }
        p.sleep(Duration::from_millis(500));
    }
}

/// Whether the TEST phone stops showing a call within `timeout`: THREE quiet looks in a row, a second apart (a
/// heads-up notification is not in the active window's dump, so one quiet look proves little: review M5). A screen
/// that cannot be read is not quiet.
pub fn wait_until_quiet(p: &mut dyn Phone, timeout: Duration) -> bool {
    let start = p.now();
    let mut quiet = 0;
    loop {
        let top_quiet = p.top_activity().is_ok_and(|t| !call_screen(&t));
        let screen_quiet = p.dump().is_ok_and(|t| {
            labelled(&t, &ANSWER_LABELS).is_none() && labelled(&t, &END_LABELS).is_none()
        });
        quiet = if top_quiet && screen_quiet {
            quiet + 1
        } else {
            0
        };
        if quiet >= 3 {
            return true;
        }
        if p.now().saturating_sub(start) >= timeout {
            return false;
        }
        p.sleep(Duration::from_secs(1));
    }
}

/// The caller on the call screen is the run's 1:1 chat: some text on the screen has the chat row's name (compared by
/// hash in memory).
fn caller_is(t: &UiTree, caller: ChatKey) -> bool {
    if caller.group {
        return false;
    }
    // A call over the chat list must not match the list's own rows: nodes inside a chat row never count.
    let in_rows: Vec<usize> = t
        .by_id(&id("contact_row_container"))
        .into_iter()
        .flat_map(|row| {
            let mut all = t.descendants(row);
            all.push(row);
            all.into_iter().map(Node::index).collect::<Vec<_>>()
        })
        .collect();
    t.iter()
        .any(|n| n.has_text() && !in_rows.contains(&n.index()) && hash_of(n) == caller.name)
}

/// End the call: tap the End control (up to three times, each checked), then see the call screen gone.
fn end_call(p: &mut dyn Phone) -> Result<()> {
    for _ in 0..3 {
        let t = p
            .dump()
            .map_err(|_| anyhow!("the call screen could not be read on the TEST phone"))?;
        let Some(end) = labelled(&t, &END_LABELS).cloned() else {
            // No End control: the call is over when the call screen is gone too.
            drop(t);
            if p.top_activity().is_ok_and(|top| !call_screen(&top)) {
                return Ok(());
            }
            p.sleep(Duration::from_millis(500));
            continue;
        };
        drop(t);
        let _ = p.tap(&end);
        if wait_for(p, SHORT, |t| labelled(t, &END_LABELS).is_none()).is_ok()
            && wait_top(p, |t| !call_screen(t), SHORT).is_ok()
        {
            return Ok(());
        }
    }
    bail!("the call did not end on the TEST phone")
}

/// Answer the ringing call ONLY when its caller is the run's 1:1 chat (`caller`), hold `hold`, end it. Any other
/// caller is never answered ([`CallFromSomeoneElse`]). When the end cannot be confirmed the error is
/// [`CallMayBeOpen`]. UNPROVEN on the real phone until the first real call.
pub fn answer_hold_and_end(p: &mut dyn Phone, hold: Duration, caller: ChatKey) -> Result<()> {
    // The ringing screen animates its "swipe up" hint, and uiautomator cannot dump a screen that never
    // goes idle (R1 2026-11-05, `VoipActivityV2`): then the caller cannot be checked and NOTHING is
    // tapped; the step hands the answer to the user ([`CallScreenUnreadable`]).
    let t = p
        .dump()
        .map_err(|_| anyhow::Error::new(CallScreenUnreadable))?;
    if !caller_is(&t, caller) {
        return Err(anyhow::Error::new(CallFromSomeoneElse));
    }
    let answer = labelled(&t, &ANSWER_LABELS).cloned();
    drop(t);
    match answer {
        Some(node) => p.tap(&node)?,
        None => {
            let top = p.top_activity().unwrap_or_default();
            if !call_screen(&top) {
                bail!("the TEST phone shows no call to answer");
            }
            p.swipe(540, 1700, 540, 700, 400)?;
        }
    }
    let outcome = (|| -> Result<()> {
        wait_for(p, SHORT, |t| labelled(t, &END_LABELS).is_some())
            .map_err(|_| anyhow!("the call was not answered on the TEST phone"))?;
        p.sleep(hold);
        let t = p
            .dump()
            .map_err(|_| anyhow!("the call screen could not be read on the TEST phone"))?;
        if labelled(&t, &END_LABELS).is_none() {
            bail!("the call was over before the hold on the TEST phone was");
        }
        Ok(())
    })();
    if end_call(p).is_err() {
        return Err(anyhow::Error::new(CallMayBeOpen));
    }
    outcome
}

/// The head of the Calls tab: a signature of the newest row (its texts and descriptions, hashed in memory) and
/// whether it is an incoming call that was not missed. `None` when the tab shows no call.
pub struct CallsHead {
    pub signature: u64,
    pub answered: bool,
}

pub fn calls_head(p: &mut dyn Phone) -> Result<Option<CallsHead>> {
    let t = home(p)?;
    let calls = bottom_tab(&t, "Calls")
        .first()
        .map(|n| (*n).clone())
        .ok_or_else(|| anyhow!("the Calls tab is not on the TEST phone's screen"))?;
    drop(t);
    let list = tap_and_wait(p, &calls, |t| t.has_id(&id("calls_recyclerView")), SHORT)
        .map_err(|_| anyhow!("the Calls tab did not open on the TEST phone"))?;
    let rows = list.by_id(&id("call_row_container"));
    let head = if let Some(newest) = rows.first() {
        let below = list.descendants(newest);
        let icon = below
            .iter()
            .find(|n| n.resource_id() == id("call_type_icon"))
            .ok_or_else(|| anyhow!("the newest call row shows no call type on the TEST phone"))?;
        let answered = if icon.desc_contains("Missed") {
            false
        } else if icon.desc_contains("Incoming") {
            true
        } else {
            bail!("the newest call row says neither incoming nor missed on the TEST phone");
        };
        let mut signature: u64 = 0;
        for n in &below {
            signature = signature.wrapping_mul(31).wrapping_add(hash_of(n));
            signature = signature
                .wrapping_mul(31)
                .wrapping_add(n.read_desc_secret().with(fnv));
        }
        Some(CallsHead {
            signature,
            answered,
        })
    } else {
        None
    };
    drop(list);
    home(p)?;
    Ok(head)
}

/// After a call: the newest row of the Calls tab is NEW (its signature differs from `before`) and was answered.
pub fn newest_call_is_new_and_answered(
    p: &mut dyn Phone,
    before: &Option<CallsHead>,
) -> Result<bool> {
    let after = calls_head(p)?;
    Ok(match (before, after) {
        (_, None) => false,
        (Some(b), Some(a)) => a.signature != b.signature && a.answered,
        (None, Some(a)) => a.answered,
    })
}

// ---- undo, tolerant (review H1): put a state back when it is set, say nothing when it is not ----

/// Under Archived? Then unarchive. `Ok(true)` when something was undone.
pub fn ensure_unarchived(p: &mut dyn Phone, key: ChatKey) -> Result<bool> {
    if locate(p, key)? == Where::Archived {
        unarchive(p, key)?;
        return Ok(true);
    }
    Ok(false)
}

/// Pinned? Then unpin.
pub fn ensure_unpinned(p: &mut dyn Phone, key: ChatKey) -> Result<bool> {
    if locate(p, key)? == Where::Nowhere {
        bail!("the chat is in neither list of the TEST phone");
    }
    if flags(p, key)?.pinned {
        pin(p, key, false)?;
        return Ok(true);
    }
    Ok(false)
}

/// Muted? Then unmute.
pub fn ensure_unmuted(p: &mut dyn Phone, key: ChatKey) -> Result<bool> {
    if locate(p, key)? == Where::Nowhere {
        bail!("the chat is in neither list of the TEST phone");
    }
    if flags(p, key)?.muted {
        mute(p, key, false)?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::super::sim::{SimFaults, WaSim};
    use super::*;

    fn key(p: &mut WaSim, group: bool, rank: usize) -> ChatKey {
        key_by_rank(p, group, rank, false).unwrap()
    }

    fn fixed(e: &anyhow::Error) {
        let s = format!("{e:#}");
        assert!(!s.contains("Sim Chat") && !s.contains("Sim Sender"), "{s}");
    }

    #[test]
    fn archive_then_unarchive_is_checked_on_the_phones_own_lists() {
        let mut p = WaSim::standard();
        let k = key(&mut p, false, 0);
        assert_eq!(locate(&mut p, k).unwrap(), Where::Main);
        archive(&mut p, k).unwrap();
        assert_eq!(locate(&mut p, k).unwrap(), Where::Archived);
        assert!(p.chat(11).archived);
        assert!(flags(&mut p, k).unwrap().archived);
        unarchive(&mut p, k).unwrap();
        assert_eq!(locate(&mut p, k).unwrap(), Where::Main);
        assert!(!p.chat(11).archived);
    }

    #[test]
    fn the_second_direct_chat_and_a_group_are_picked_by_rank_and_kind() {
        let mut p = WaSim::standard();
        let direct1 = key(&mut p, false, 1);
        archive(&mut p, direct1).unwrap();
        assert!(p.chat(13).archived && !p.chat(11).archived);
        let group = key(&mut p, true, 0);
        archive(&mut p, group).unwrap();
        assert!(p.chat(12).archived);
        let in_archive = key_by_rank(&mut p, true, 0, true).unwrap();
        assert_eq!(in_archive, group);
        assert!(key_by_rank(&mut p, true, 1, false).is_err());
    }

    #[test]
    fn an_act_that_does_not_take_effect_is_an_error_and_nothing_changed() {
        let mut p = WaSim::standard();
        let k = key(&mut p, false, 0);
        p.faults = SimFaults {
            dead_toolbar: true,
            ..Default::default()
        };
        let e = archive(&mut p, k).unwrap_err();
        fixed(&e);
        assert!(format!("{e}").contains("did not take effect"), "{e}");
        assert!(!p.chat(11).archived);
        assert!(pin(&mut p, k, true).is_err());
        assert!(mute(&mut p, k, true).is_err());
        assert!(!p.chat(11).pinned && !p.chat(11).muted);
    }

    #[test]
    fn an_act_that_closes_the_selection_but_changes_nothing_is_not_a_pass() {
        let mut p = WaSim::standard();
        let k = key(&mut p, false, 0);
        p.faults = SimFaults {
            phantom_acts: true,
            ..Default::default()
        };
        let e = archive(&mut p, k).unwrap_err();
        fixed(&e);
        assert!(format!("{e}").contains("not under Archived"), "{e}");
        for r in [pin(&mut p, k, true), mute(&mut p, k, true)] {
            assert!(r.is_err());
        }
        assert!(p.chats.iter().all(|c| !c.archived && !c.pinned && !c.muted));
    }

    #[test]
    fn an_unreadable_screen_is_never_a_pass() {
        let mut p = WaSim::standard();
        let k = key(&mut p, false, 0);
        p.faults.unreadable = true;
        for e in [
            archive(&mut p, k).unwrap_err(),
            locate(&mut p, k).unwrap_err(),
            unarchive(&mut p, k).unwrap_err(),
            group_counts(&mut p, k).unwrap_err(),
            keep_chats_archived(&mut p).map(|_| ()).unwrap_err(),
            calls_head(&mut p).map(|_| ()).unwrap_err(),
        ] {
            fixed(&e);
        }
        assert!(!wait_for_ring(&mut p, Duration::from_secs(2)));
        assert!(!p.chat(11).archived);
    }

    #[test]
    fn the_registration_block_screen_stops_every_act_with_one_sentence() {
        let mut p = WaSim::standard();
        let k = key(&mut p, false, 0);
        p.faults.blocked = true;
        for e in [
            archive(&mut p, k).unwrap_err(),
            pin(&mut p, k, true).unwrap_err(),
            mute(&mut p, k, true).unwrap_err(),
            group_counts(&mut p, k).unwrap_err(),
            keep_chats_archived(&mut p).map(|_| ()).unwrap_err(),
            calls_head(&mut p).map(|_| ()).unwrap_err(),
        ] {
            assert!(format!("{e:#}").contains(BLOCKED), "{e:#}");
        }
        assert!(p.chats.iter().all(|c| !c.archived && !c.pinned && !c.muted));
    }

    #[test]
    fn a_press_the_phone_ignores_once_is_made_again_only_while_the_row_stands() {
        let mut p = WaSim::standard();
        let k = key(&mut p, false, 0);
        p.faults.ignore_presses = 1;
        archive(&mut p, k).unwrap();
        assert!(p.chat(11).archived);
        // Every press ignored: the act fails closed and nothing happened.
        let mut q = WaSim::standard();
        let k = key(&mut q, false, 0);
        q.faults.ignore_presses = 99;
        let e = archive(&mut q, k).unwrap_err();
        fixed(&e);
        assert!(!q.chat(11).archived);
    }

    #[test]
    fn no_chat_row_is_ever_tapped_and_the_hold_is_the_long_one() {
        let mut p = WaSim::standard();
        let k = key(&mut p, false, 0);
        archive(&mut p, k).unwrap();
        let first = key_by_rank(&mut p, false, 0, false).unwrap();
        pin(&mut p, first, true).unwrap();
        for a in &p.acts {
            assert!(
                !a.contains("contact_row_container 50"),
                "a row was tapped: {a}"
            );
            assert!(!a.contains("menuitem_conversations_delete"), "{a}");
        }
        assert!(
            p.acts
                .iter()
                .any(|a| a.contains("contact_row_container 2000"))
        );
    }

    #[test]
    fn pin_and_mute_are_read_back_from_the_toolbar() {
        let mut p = WaSim::standard();
        let k = key(&mut p, false, 0);
        pin(&mut p, k, true).unwrap();
        assert!(p.chat(11).pinned && flags(&mut p, k).unwrap().pinned);
        // pinning twice is refused: nothing to do, nothing done
        assert!(pin(&mut p, k, true).is_err());
        mute(&mut p, k, true).unwrap();
        assert!(p.chat(11).muted && flags(&mut p, k).unwrap().muted);
        assert!(mute(&mut p, k, true).is_err());
        mute(&mut p, k, false).unwrap();
        pin(&mut p, k, false).unwrap();
        assert!(!p.chat(11).muted && !p.chat(11).pinned);
    }

    #[test]
    fn a_groups_members_and_admins_are_counted_in_memory() {
        let mut p = WaSim::standard();
        let g = key(&mut p, true, 0);
        assert_eq!(group_counts(&mut p, g).unwrap(), (3, 2));
        // back on the main screen, nothing selected
        assert!(on_home(&p.dump().unwrap()));
        // a bigger group scrolls
        let mut big = WaSim::standard().with_members(
            (0..10)
                .map(|i| (200 + i, i == 1 || i == 7))
                .collect::<Vec<_>>(),
        );
        let g = key(&mut big, true, 0);
        assert_eq!(group_counts(&mut big, g).unwrap(), (10, 2));
    }

    #[test]
    fn a_member_list_that_is_short_of_the_title_or_a_chat_that_is_no_group_is_an_error() {
        let mut p = WaSim::standard();
        p.faults.short_member_list = true;
        let g = key(&mut p, true, 0);
        let e = group_counts(&mut p, g).unwrap_err();
        fixed(&e);
        assert!(format!("{e}").contains("completely"), "{e}");
        let mut q = WaSim::standard();
        let d = key(&mut q, false, 0);
        let e = group_counts(&mut q, d).unwrap_err();
        assert!(format!("{e}").contains("no group"), "{e}");
    }

    #[test]
    fn keep_chats_archived_is_read_by_its_row_and_never_toggled() {
        let mut off = WaSim::standard();
        assert!(!keep_chats_archived(&mut off).unwrap());
        assert!(!off.acts.iter().any(|a| a.contains("TOGGLE")));
        let mut on = WaSim::standard().keep_archived(true);
        assert!(keep_chats_archived(&mut on).unwrap());
        assert!(!on.acts.iter().any(|a| a.contains("TOGGLE")));
        assert!(on_home(&on.dump().unwrap()));
    }

    fn caller_key(p: &mut WaSim) -> ChatKey {
        key_by_rank(p, false, 0, false).unwrap()
    }

    #[test]
    fn a_ringing_call_is_seen_answered_held_and_ended_when_the_caller_is_the_chat() {
        let mut p = WaSim::standard().ring_at(Duration::from_secs(3));
        let who = caller_key(&mut p);
        let before = calls_head(&mut p).unwrap();
        assert!(before.is_none());
        assert!(wait_for_ring(&mut p, Duration::from_secs(10)));
        assert!(p.is_ringing());
        answer_hold_and_end(&mut p, Duration::from_secs(15), who).unwrap();
        assert!(!p.in_call() && !p.is_ringing());
        assert_eq!(p.calls_logged(), 1);
        assert!(newest_call_is_new_and_answered(&mut p, &before).unwrap());
    }

    #[test]
    fn a_call_from_someone_else_is_never_answered() {
        let mut p = WaSim::standard().ring_at(Duration::ZERO).faults(SimFaults {
            stranger_calls: true,
            ..Default::default()
        });
        let who = caller_key(&mut p);
        assert!(wait_for_ring(&mut p, Duration::from_secs(3)));
        let e = answer_hold_and_end(&mut p, Duration::from_secs(5), who).unwrap_err();
        assert!(e.downcast_ref::<CallFromSomeoneElse>().is_some(), "{e}");
        fixed(&e);
        assert!(p.is_ringing() && !p.in_call(), "the call was answered");
        assert!(
            !p.acts
                .iter()
                .any(|a| a.contains("press") && a.contains(" 50")),
            "{:?}",
            p.acts
        );
    }

    #[test]
    fn a_call_that_cannot_be_ended_is_reported_as_possibly_open() {
        let mut p = WaSim::standard().ring_at(Duration::ZERO).faults(SimFaults {
            end_does_nothing: true,
            ..Default::default()
        });
        let who = caller_key(&mut p);
        assert!(wait_for_ring(&mut p, Duration::from_secs(3)));
        let e = answer_hold_and_end(&mut p, Duration::from_secs(1), who).unwrap_err();
        assert!(e.downcast_ref::<CallMayBeOpen>().is_some(), "{e}");
        assert!(p.in_call());
    }

    #[test]
    fn no_ring_is_a_no_and_a_call_answered_by_swipe_works_too() {
        let mut p = WaSim::standard();
        assert!(!wait_for_ring(&mut p, Duration::from_secs(5)));
        let mut never = WaSim::standard()
            .ring_at(Duration::from_secs(1))
            .faults(SimFaults {
                never_rings: true,
                ..Default::default()
            });
        let who = caller_key(&mut never);
        assert!(!wait_for_ring(&mut never, Duration::from_secs(5)));
        assert!(answer_hold_and_end(&mut never, Duration::from_secs(1), who).is_err());
        // The key is taken before the ring: a swipe up on the list during a ring would answer it (the fallback gesture).
        let mut sw = WaSim::standard()
            .ring_at(Duration::from_secs(60))
            .faults(SimFaults {
                swipe_to_answer: true,
                ..Default::default()
            });
        let who = caller_key(&mut sw);
        assert!(wait_for_ring(&mut sw, Duration::from_secs(80)));
        answer_hold_and_end(&mut sw, Duration::from_secs(2), who).unwrap();
        assert_eq!(sw.calls_logged(), 1);
    }

    #[test]
    fn the_calls_tab_must_show_a_new_row_and_an_answered_one() {
        let mut p = WaSim::standard();
        assert!(calls_head(&mut p).unwrap().is_none());
        assert!(!newest_call_is_new_and_answered(&mut p, &None).unwrap());
        let mut m = WaSim::standard().ring_at(Duration::ZERO).faults(SimFaults {
            log_missed: true,
            ..Default::default()
        });
        let who = caller_key(&mut m);
        assert!(wait_for_ring(&mut m, Duration::from_secs(2)));
        answer_hold_and_end(&mut m, Duration::from_secs(1), who).unwrap();
        assert!(!newest_call_is_new_and_answered(&mut m, &None).unwrap());
        // An old answered row is not the new call.
        let mut o = WaSim::standard().ring_at(Duration::ZERO);
        let who = caller_key(&mut o);
        assert!(wait_for_ring(&mut o, Duration::from_secs(2)));
        answer_hold_and_end(&mut o, Duration::from_secs(1), who).unwrap();
        let now = calls_head(&mut o).unwrap();
        assert!(!newest_call_is_new_and_answered(&mut o, &now).unwrap());
    }

    #[test]
    fn a_row_that_moved_under_the_finger_is_never_acted_on() {
        let mut p = WaSim::standard();
        let k = key(&mut p, false, 0);
        p.faults.wrong_row_press = true;
        let e = archive(&mut p, k).unwrap_err();
        fixed(&e);
        assert!(format!("{e}").contains("not the one asked for"), "{e}");
        assert!(p.chats.iter().all(|c| !c.archived && !c.pinned && !c.muted));
        for r in [
            pin(&mut p, k, true),
            mute(&mut p, k, true),
            flags(&mut p, k).map(|_| ()),
        ] {
            assert!(r.is_err());
        }
        assert!(p.chats.iter().all(|c| !c.archived && !c.pinned && !c.muted));
    }

    #[test]
    fn without_a_selected_mark_the_count_and_the_unmoved_list_decide() {
        let mut p = WaSim::standard();
        let k = key(&mut p, false, 0);
        p.faults.no_selected_flag = true;
        archive(&mut p, k).unwrap();
        assert!(p.chat(11).archived);
        // ... and a wrong row is still caught by the list that no longer matches? Here the row below is selected
        // with no mark: the count says one and the list is the same, so the press cannot be told wrong. The mark is
        // what proves it; the sim's wrong press WITH a mark is the case above.
        unarchive(&mut p, k).unwrap();
    }

    #[test]
    fn two_chats_of_one_name_and_kind_cannot_be_told_apart() {
        let mut p = WaSim::standard();
        p.chats[2].token = 11;
        let e = locate(
            &mut p,
            ChatKey {
                name: 0,
                group: false,
            },
        )
        .unwrap_err();
        assert!(format!("{e}").contains("cannot be told apart"), "{e}");
    }

    #[test]
    fn a_failed_act_can_be_undone_by_the_tolerant_helpers() {
        let mut p = WaSim::standard();
        let k = key(&mut p, false, 0);
        assert!(!ensure_unarchived(&mut p, k).unwrap());
        archive(&mut p, k).unwrap();
        assert!(ensure_unarchived(&mut p, k).unwrap());
        assert!(!p.chat(11).archived);
        pin(&mut p, k, true).unwrap();
        mute(&mut p, k, true).unwrap();
        assert!(ensure_unmuted(&mut p, k).unwrap());
        assert!(ensure_unpinned(&mut p, k).unwrap());
        assert!(!p.chat(11).pinned && !p.chat(11).muted);
    }

    #[test]
    fn the_home_screen_is_the_chats_tab_even_when_another_tab_was_left_open() {
        let mut p = WaSim::standard();
        // Calls tab open: home() taps Chats back.
        let t = p.dump().unwrap();
        let calls = t
            .find(|n| n.clickable() && n.desc_contains("Calls"))
            .first()
            .map(|n| (*n).clone())
            .unwrap();
        drop(t);
        p.tap(&calls).unwrap();
        let t = home(&mut p).unwrap();
        assert!(chats_selected(&t));
    }

    #[test]
    fn a_chat_key_prints_nothing_of_the_name() {
        let mut p = WaSim::standard();
        let k = key(&mut p, false, 0);
        assert_eq!(format!("{k:?}"), "ChatKey(..)");
    }
}
