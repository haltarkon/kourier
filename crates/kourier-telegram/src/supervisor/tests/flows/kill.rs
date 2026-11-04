//! TG-3 C10a (critic M8, brief §4.7): the kill harness. A CHILD process (this test binary
//! re-executed by `kourier_tdlib::spawn::spawn_self`, armed by `KOURIER_FAILPOINT`, the one
//! variable it passes) runs the REAL actor over a `ScriptedTd` on an account directory the
//! parent made; the parent reads what the child left after it was aborted.
//!
//! The world (synthetic, the scrub's values): three private chats 77, 78, 79 and a STREAM of
//! [`PER_CHAT`] messages each, and (fix round TF9) a SUPERGROUP [`SG`] whose stream skips one id
//! (`SG_STREAM`: 3 is a deleted message), so a kill leaves its id trigger, its quiet-wait start
//! decision and the page that finds nothing in the loops; the stream is pushed after Online in
//! two rounds (the first message of each chat, then the rest; each round in the core before the
//! next). "TDLib" keeps a processed log
//! (`Script::processed_log`): a pushed update is written there, synced, before the actor can see
//! it (TDLib's pts save). A later child models a restarted TDLib from that log: what it processed
//! is its history (each chat's `last_message`, its unread count, `getChatHistory` pages from the
//! log); what it never processed comes as live updates again (its difference); nothing it
//! processed is pushed again (RC3: no re-delivery in this world). The child's fake core appends
//! every durable envelope's messages to a synced core log BEFORE it acks (the core's commit), so
//! the core's state survives the abort. A child ends normally once the core holds the whole
//! stream and a Shutdown ended the actor.
//!
//! C10b adds the loops (brief §4.7, critic B1 rule 4, RC3): a [`Mode`] the parent sends with the
//! directory makes the recovery child push its live messages (the stream's unprocessed tail plus
//! [`EXTRA`] NEW ones per chat) before `loadChats` answers ([`Mode::Early`]) or while the first
//! reconcile page is held ([`Mode::Reconcile`]), or push one committed message again through the
//! stream ([`Mode::Redeliver`]). After every kill the parent checks `clean_end` and that each
//! chat's cursor equals the newest message its rows hold (`core_outbox` + the core log).
//!
//! TG-4 K6a adds the supergroup's id trigger under SIGKILL ([`has_sg_tail`] and
//! `every_supergroup_tail_after_a_kill_comes_back_through_the_id_trigger`): a kill that leaves the
//! supergroup a tail above its cursor, recovered in [`Mode::Early`], where the live supergroup
//! message moves the cursor above `td_last` before the start decision, so only the id trigger's
//! gap fetches the tail.
//!
//! TG-4 K6b adds the waiting path under SIGKILL ([`Mode::WaitChat`] and
//! `every_chat_that_waited_for_its_getchat_after_a_kill_comes_back_through_the_fresh_load`): the
//! child is a FIRST LINK (a login, so the chat machine's bound applies before the link) with the
//! bound lowered through `Timings::learn_bound` to [`WAIT_BOUND`]: the objects TDLib's first
//! difference tells before the link are the users and the private chats, then the supergroup and
//! its chat LAST, so the supergroup is the first object past the bound (dropped), its chat is
//! asked with `getChat` at the link, and "TDLib" holds that answer until the whole stream was
//! pushed: the supergroup's messages WAIT in the message machine, the private chats' (cursor 0)
//! are held for their fresh load. The world is synthetic (TDLib's order never leaves a chat
//! unknown for want of one answer: the answer is composed before the pushes it is held behind).
//!
//! TG-4 K7 adds the one-commit fresh load under SIGKILL ([`Mode::FreshHeld`], [`Mode::FreshEarly`],
//! [`Mode::FreshPlain`] and the two per-point loop modules at the end): a RESUMED child over an
//! account with an empty receive state (every private chat has stored cursor 0), whose TDLib
//! holds, per private chat, two READ older messages, three UNREAD ones and the last of them (the
//! newest unread), and which gets ONE live message into one chat (`FRESH_LIVE`): in
//! `FreshHeld` while that chat's first page is held (its plan is open), in `FreshEarly` before the
//! held `loadChats` answer (the chat is not even known yet). After EVERY kill, per chat, either
//! the cursor is 0 and no row of the chat is in `core_outbox` or the core log, or the rows hold
//! the WHOLE batch (the last message, the page's rows and the held live message, nothing older)
//! and the cursor is the newest of them ([`check_fresh`]); after the recovery ([`Mode::FreshPlain`],
//! an unarmed resumed child, TDLib restarted over what it processed) the core holds exactly that
//! world and no message below the page.
//!
//! TG-4 fix round U7 makes the preconditions of the loops that need a TAIL (a chat whose cursor lags
//! what TDLib processed) deterministic BY CONSTRUCTION ([`Mode::Paced`], [`killed_with_tail`],
//! [`run_ahead`]): the first child pushes the stream ONE message at a time and pushes the next only
//! after the previous one is in the core's log (its commit came first), so the n-th commit that
//! holds rows holds exactly one message, whatever the machine's speed, and a kill inside it leaves
//! that message processed (TDLib's log holds it before the actor sees it) and uncommitted; a tail of
//! two is TDLib running one message AHEAD of what the killed child saw, which the parent states in
//! the log itself. Before, the tail came from how the actor's group commit happened to split a
//! burst of pushes, which a loaded machine splits differently: "no kill left the state the test
//! needs".
//!
//! Every value is synthetic; nothing a test asserts on is printed (counts and failpoint names).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::Path;

use kourier_db::rusqlite::OptionalExtension as _;
use serde_json::Value;

use super::*;

/// The test the child runs (`spawn_self` passes `--exact` with this name).
const KILL_CHILD: &str = "supervisor::tests::flows::kill::kill_child";
/// The private chats of the world (each its peer's user id).
const CHATS: [i64; 3] = [77, 78, 79];
/// The supergroup of the world (TF9; supergroup 777, its chat id is TDLib's `-100..` form).
const SG: i64 = -1_000_000_000_777;
/// The supergroup's object id.
const SG_ID: i64 = 777;
/// The supergroup's stream (server ids): 3 is a message deleted before the world begins, so the
/// message after it opens an id-jump gap whose one page finds nothing.
const SG_STREAM: [i64; 3] = [1, 2, 4];
/// The supergroup's NEW message of [`Mode::Early`] / [`Mode::Reconcile`] (no gap: 4 + 1).
const SG_EXTRA: [i64; 1] = [5];
/// Every chat of the world.
const ALL: [i64; 4] = [77, 78, 79, SG];
/// Messages per private chat in the stream.
const PER_CHAT: i64 = 4;
/// TDLib's processed log, in the account directory.
const PROCESSED: &str = "processed.jsonl";
/// The fake core's committed messages, one `<chat> <local>` line each.
const CORE: &str = "core.log";
/// A spec that arms nothing (the recovery child: a normal run).
const UNARMED: &str = "tg-never-hit:1";
/// The full id of server message `id` (TDLib's `id << 20`).
const STEP: i64 = 1 << 20;
/// NEW messages per chat a recovery child in [`Mode::Early`] / [`Mode::Reconcile`] pushes after
/// the stream (ids `PER_CHAT + 1 ..`).
const EXTRA: i64 = 1;
/// The most hits of one failpoint a loop tries before it calls the world broken (each point is
/// hit a few times per child here).
const MAX_HITS: usize = 40;
/// The chat machine's bound of a [`Mode::WaitChat`] child: the objects learnt before the supergroup
/// (TG-4 K6b): the own user (the login's code answer tells it first), the three users and the
/// three private chats. The supergroup object and its chat are told after them, so both come past
/// the bound.
const WAIT_BOUND: usize = 1 + 2 * CHATS.len();
/// The fresh-link world (TG-4 K7), per private chat, as TDLib holds it before the link: these
/// older messages are READ (`last_read_inbox_message_id` is the newest of them) ...
const FRESH_READ: [i64; 2] = [1, 2];
/// ... and these are UNREAD, the newest being the chat's last message (TDLib's `unread_count`
/// counts it, so the fresh-link page is exact, U5: `unread - 1` = two messages below the last
/// one, the two unread ones; no read message, neither the newest read one nor the older, reaches
/// the core).
const FRESH_UNREAD: [i64; 3] = [3, 4, 5];
/// The live message of the fresh-link world: (chat, server id), above the last message.
const FRESH_LIVE: (i64, i64) = (77, 6);

/// What a child does beside the plain run (sent over fd 3 as the first line, the directory the
/// second).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Connect, Online, push the unprocessed stream.
    Plain,
    /// The main list's first `loadChats` answer is held until the child pushed the unprocessed
    /// stream and the [`EXTRA`] messages (B1 rule 4: live messages before `loadChats`).
    Early,
    /// Every `getChatHistory` answer is held until the first one was asked; the child pushes the
    /// unprocessed stream and the [`EXTRA`] messages, then releases (a live message during a
    /// reconcile).
    Reconcile,
    /// After the unprocessed stream, message `(chat, id)` (already committed) once more: TDLib's
    /// difference re-delivering it (RC3).
    Redeliver(i64, i64),
    /// A FIRST LINK over an empty directory (TG-4 K6b): the child logs in, TDLib tells the objects
    /// before the link with the supergroup and its chat past the lowered bound ([`WAIT_BOUND`]),
    /// the `getChat` the link asks for the chat is held until the whole stream was pushed (the
    /// supergroup's messages wait for it), then released.
    WaitChat,
    /// The fresh-link world (TG-4 K7, [`FRESH_UNREAD`]): a resumed child over an account with no
    /// receive state; EVERY `getChatHistory` answer is held until the first one was asked (the
    /// first fresh page: its plan is open), the child then pushes the live message
    /// ([`FRESH_LIVE`], unless TDLib processed it before a kill) and releases.
    FreshHeld,
    /// The fresh-link world with the live message BEFORE the lists load: the main list's first
    /// `loadChats` answer is held until the child pushed it (it waits for its chat, then for the
    /// batch), and TDLib's answer tells the chat with that message as its last.
    FreshEarly,
    /// The recovery of the fresh-link world: nothing held; the live message comes after Online
    /// unless TDLib processed it.
    FreshPlain,
    /// [`Mode::Plain`] with the stream pushed ONE message at a time (U7): the next is pushed only
    /// after the previous one is in the core's log, so every commit with rows holds one message
    /// and a kill inside the n-th leaves exactly that message processed and uncommitted.
    Paced,
}

impl Mode {
    fn line(self) -> String {
        match self {
            Self::Plain => "plain".to_owned(),
            Self::Early => "early".to_owned(),
            Self::Reconcile => "reconcile".to_owned(),
            Self::Redeliver(chat, id) => format!("redeliver {chat} {id}"),
            Self::WaitChat => "waitchat".to_owned(),
            Self::FreshHeld => "freshheld".to_owned(),
            Self::FreshEarly => "freshearly".to_owned(),
            Self::FreshPlain => "freshplain".to_owned(),
            Self::Paced => "paced".to_owned(),
        }
    }

    fn parse(line: &str) -> Option<Self> {
        let mut words = line.split(' ');
        match words.next()? {
            "plain" => Some(Self::Plain),
            "early" => Some(Self::Early),
            "reconcile" => Some(Self::Reconcile),
            "waitchat" => Some(Self::WaitChat),
            "freshheld" => Some(Self::FreshHeld),
            "freshearly" => Some(Self::FreshEarly),
            "freshplain" => Some(Self::FreshPlain),
            "paced" => Some(Self::Paced),
            "redeliver" => Some(Self::Redeliver(
                words.next()?.parse().ok()?,
                words.next()?.parse().ok()?,
            )),
            _ => None,
        }
    }

    /// Whether the child pushes the [`EXTRA`] messages.
    fn extra(self) -> bool {
        matches!(self, Self::Early | Self::Reconcile)
    }

    /// Whether the child lives in the fresh-link world (TG-4 K7).
    fn fresh(self) -> bool {
        matches!(self, Self::FreshHeld | Self::FreshEarly | Self::FreshPlain)
    }
}

/// The stream in TDLib's order: (chat, server id), round-robin over the chats.
fn stream() -> Vec<(i64, i64)> {
    (1..=PER_CHAT)
        .flat_map(|id| {
            CHATS
                .iter()
                .map(move |&chat| (chat, id))
                .chain(SG_STREAM.contains(&id).then_some((SG, id)))
        })
        .collect()
}

/// The author of a chat's messages (the supergroup's are the first private chat's peer).
fn author(chat: i64) -> i64 {
    if chat == SG { CHATS[0] } else { chat }
}

fn message(chat: i64, id: i64) -> Value {
    json!({"@type": "message", "id": id * STEP, "chat_id": chat, "is_outgoing": false,
        "sender_id": {"@type": "messageSenderUser", "user_id": author(chat)},
        "date": 1_700_000_100 + id, "edit_date": 0, "media_album_id": "0",
        "content": {"@type": "messageText", "text": {"@type": "formattedText",
            "text": format!("synthetic text {}", chat * 100 + id), "entities": []}}})
}

fn new_message(chat: i64, id: i64) -> Value {
    json!({"@type": "updateNewMessage", "message": message(chat, id)})
}

fn user(id: i64) -> Value {
    json!({"@type": "updateUser", "user": {"@type": "user", "id": id, "first_name": "Bo",
        "last_name": "Peer", "phone_number": "", "is_contact": true,
        "type": {"@type": "userTypeRegular"}}})
}

/// The chat as TDLib knows it: its newest processed message and every processed one unread
/// (nothing in this world is read).
fn chat(id: i64, processed: &[i64]) -> Value {
    let last = processed.iter().max().map(|&m| message(id, m));
    let (kind, title) = if id == SG {
        (
            json!({"@type": "chatTypeSupergroup", "supergroup_id": SG_ID, "is_channel": false}),
            "Group 9",
        )
    } else {
        (
            json!({"@type": "chatTypePrivate", "user_id": id}),
            "Bo Peer",
        )
    };
    json!({"@type": "updateNewChat", "chat": {"@type": "chat", "id": id,
        "type": kind, "title": title, "photo": null,
        "positions": [{"@type": "chatPosition", "list": {"@type": "chatListMain"},
            "order": format!("{}", 9_000_000_000_000_000_000_i64 - id), "is_pinned": false}],
        "last_message": last, "has_protected_content": false, "is_marked_as_unread": false,
        "unread_count": processed.len(), "last_read_inbox_message_id": 0,
        "last_read_outbox_message_id": 0, "unread_mention_count": 0,
        "notification_settings": {"@type": "chatNotificationSettings",
            "use_default_mute_for": true, "mute_for": 0}, "message_auto_delete_time": 0}})
}

/// (chat, server id) of every message TDLib processed in `dir`, in its order.
fn processed(dir: &Path) -> Vec<(i64, i64)> {
    kourier_tdlib::scripted::processed(&dir.join(PROCESSED))
        .iter()
        .filter(|u| u["@type"] == "updateNewMessage")
        .filter_map(|u| {
            let m = &u["message"];
            Some((m["chat_id"].as_i64()?, m["id"].as_i64()? / STEP))
        })
        .collect()
}

/// (chat, local) of every message the fake core committed in `dir` (a torn last line dropped).
fn committed(dir: &Path) -> Vec<(i64, String)> {
    let Ok(text) = std::fs::read_to_string(dir.join(CORE)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| {
            let (chat, local) = l.split_once(' ')?;
            Some((chat.parse().ok()?, local.to_owned()))
        })
        .collect()
}

/// The supergroup object as TDLib tells it (`updateSupergroup`'s, `getSupergroup`'s answer).
fn supergroup_object() -> Value {
    json!({"@type": "supergroup",
        "id": SG_ID, "usernames": null, "date": 1_700_000_000, "member_count": 3,
        "is_channel": false, "is_broadcast_group": false, "is_forum": false,
        "is_direct_messages_group": false})
}

/// The idle pump ticks (20 ms each) a [`Mode::FreshHeld`] child waits after it pushed the live
/// message, before it releases the held page: time for the actor to take the message alone.
const SETTLE_TICKS: usize = 10;

/// The messages TDLib holds in `chat` in the fresh-link world (TG-4 K7), server ids: the read ones,
/// the unread ones and, when TDLib has the live message ([`FRESH_LIVE`]), that.
fn fresh_ids(chat: i64, with_live: bool) -> Vec<i64> {
    let mut ids: Vec<i64> = FRESH_READ.iter().chain(&FRESH_UNREAD).copied().collect();
    if with_live && chat == FRESH_LIVE.0 {
        ids.push(FRESH_LIVE.1);
    }
    ids
}

/// The chat as TDLib tells it in the fresh-link world: its newest message as the last one, the
/// read mark at the newest read message, `unread_count` the messages above it.
fn fresh_chat(id: i64, ids: &[i64]) -> Value {
    let mark = FRESH_READ.iter().max().copied().unwrap_or(0);
    let mut told = chat(id, ids);
    told["chat"]["last_read_inbox_message_id"] = json!(mark * STEP);
    told["chat"]["unread_count"] = json!(ids.iter().filter(|&&m| m > mark).count());
    told
}

/// The script of the fresh-link world (TG-4 K7) over `dir`'s processed log: the main list's first
/// page holds the users and the three private chats (each with its last message and unread
/// count; the live message is the last of its chat when TDLib processed it before the answer:
/// [`Mode::FreshEarly`] pushes it before the held answer, a restart finds it in the log), every
/// other list page is the end (404); `getChatHistory` honours `from_message_id` and `limit` over
/// what TDLib holds (the fresh pages ask `limit <= 4`, the reconcile 50; anything else is an
/// empty page), held while the mode says; `getChat` answers the chat.
fn fresh_script(dir: &Path, mode: Mode) -> Script {
    let told_live = mode == Mode::FreshEarly || processed(dir).contains(&FRESH_LIVE);
    let ids = |c: i64| fresh_ids(c, told_live);
    let mut first: Vec<Value> = CHATS.iter().map(|&c| user(c)).collect();
    first.extend(CHATS.iter().map(|&c| fresh_chat(c, &ids(c))));
    first.push(ok());
    let main = Rule::nth("loadChats", 1, first);
    let mut rules = vec![
        if mode == Mode::FreshEarly {
            main.deferring()
        } else {
            main
        },
        Rule::new("loadChats", vec![td_error(404, "Not Found")]),
    ];
    for c in CHATS {
        rules.push(Rule::when(
            "getChat",
            "/chat_id",
            json!(c),
            vec![fresh_chat(c, &ids(c))["chat"].clone()],
        ));
    }
    let hold = |rule: Rule| {
        if mode == Mode::FreshHeld {
            rule.deferring()
        } else {
            rule
        }
    };
    let limits = (1..=8).chain([i64::from(crate::sync::reconcile::PAGE_LIMIT)]);
    for c in CHATS {
        let all = ids(c);
        let newest = all.iter().max().copied().unwrap_or(0);
        for limit in limits.clone() {
            for from in 0..=FRESH_LIVE.1 + 1 {
                let top = if from == 0 { newest + 1 } else { from };
                let mut below: Vec<i64> = all.iter().copied().filter(|&m| m < top).collect();
                below.sort_unstable_by(|a, b| b.cmp(a));
                below.truncate(usize::try_from(limit).expect("a limit"));
                let msgs: Vec<Value> = below.iter().map(|&m| message(c, m)).collect();
                rules.push(hold(Rule::when_all(
                    "getChatHistory",
                    &[
                        ("/chat_id", json!(c)),
                        ("/from_message_id", json!(from * STEP)),
                        ("/limit", json!(limit)),
                    ],
                    vec![json!({"@type": "messages", "total_count": msgs.len(), "messages": msgs})],
                )));
            }
        }
    }
    rules.push(hold(Rule::new(
        "getChatHistory",
        vec![json!({"@type": "messages", "total_count": 0, "messages": []})],
    )));
    Script {
        processed_log: Some(dir.join(PROCESSED)),
        ..ready_script(rules)
    }
}

/// The script of a TDLib restarted over `dir`'s processed log: the main list's first page holds
/// the three chats, every other list page is the end (404); a chat's history is what it
/// processed, newest first, below the page's `from_message_id` (0: from the newest; a page from
/// any id of the world, the [`EXTRA`] ones too, is answered); `getChat` answers the chat. `mode`
/// defers the answers it holds; `also` are messages this TDLib will have processed by the time it
/// answers (the live messages a [`Mode::Early`] child pushes before the held `loadChats`).
/// [`Mode::WaitChat`] is a login's script: the objects come with the `getMe` answer (before the
/// link, in the order of the module doc), `loadChats` only says `ok`, and the supergroup chat's
/// `getChat` answer is the held one.
fn script(dir: &Path, mode: Mode, also: &[(i64, i64)]) -> Script {
    if mode.fresh() {
        return fresh_script(dir, mode);
    }
    let mut done = processed(dir);
    done.extend_from_slice(also);
    let of = |c: i64| -> Vec<i64> {
        done.iter()
            .filter(|(chat, _)| *chat == c)
            .map(|(_, id)| *id)
            .collect()
    };
    let mut first: Vec<Value> = CHATS.iter().map(|&c| user(c)).collect();
    first.push(json!({"@type": "updateSupergroup", "supergroup": supergroup_object()}));
    first.extend(ALL.iter().map(|&c| chat(c, &of(c))));
    first.push(ok());
    // A first link: TDLib's first difference tells the users and private chats, the supergroup and
    // its chat last (the bound lands on the supergroup); the lists then only say `ok`.
    let before_link: Vec<Value> = CHATS
        .iter()
        .map(|&c| user(c))
        .chain(CHATS.iter().map(|&c| chat(c, &of(c))))
        .chain([
            json!({"@type": "updateSupergroup", "supergroup": supergroup_object()}),
            chat(SG, &of(SG)),
        ])
        .collect();
    if mode == Mode::WaitChat {
        first = vec![ok()];
    }
    let main = Rule::nth("loadChats", 1, first);
    let mut rules = vec![
        if mode == Mode::Early {
            main.deferring()
        } else {
            main
        },
        Rule::new("loadChats", vec![td_error(404, "Not Found")]),
    ];
    for c in ALL {
        let answer = Rule::when(
            "getChat",
            "/chat_id",
            json!(c),
            vec![chat(c, &of(c))["chat"].clone()],
        );
        rules.push(if mode == Mode::WaitChat && c == SG {
            answer.deferring()
        } else {
            answer
        });
    }
    rules.push(Rule::when(
        "getSupergroup",
        "/supergroup_id",
        json!(SG_ID),
        vec![supergroup_object()],
    ));
    let hold = |rule: Rule| {
        if mode == Mode::Reconcile {
            rule.deferring()
        } else {
            rule
        }
    };
    for c in ALL {
        let ids = of(c);
        let newest = ids.iter().max().copied().unwrap_or(0);
        for from in 0..=PER_CHAT + EXTRA {
            let top = if from == 0 { newest + 1 } else { from };
            let mut below: Vec<i64> = ids.iter().copied().filter(|&m| m < top).collect();
            below.sort_unstable_by(|a, b| b.cmp(a));
            let msgs: Vec<Value> = below.iter().map(|&m| message(c, m)).collect();
            rules.push(hold(Rule::when_all(
                "getChatHistory",
                &[
                    ("/chat_id", json!(c)),
                    ("/from_message_id", json!(from * STEP)),
                ],
                vec![json!({"@type": "messages", "total_count": msgs.len(), "messages": msgs})],
            )));
        }
    }
    rules.push(hold(Rule::new(
        "getChatHistory",
        vec![json!({"@type": "messages", "total_count": 0, "messages": []})],
    )));
    let base = if mode == Mode::WaitChat {
        let mut login = login_script(rules);
        // The objects ride the `getMe` answer: the actor reads them, in the stream's order, before
        // the answer that makes the `AccountReady` (so before the link, whatever the thread timing).
        let me = login
            .rules
            .iter_mut()
            .find(|r| r.on == "getMe")
            .expect("the fixture's getMe rule");
        let mut reply = before_link;
        reply.append(&mut me.reply);
        me.reply = reply;
        login
    } else {
        ready_script(rules)
    };
    Script {
        processed_log: Some(dir.join(PROCESSED)),
        ..base
    }
}

/// The chat of a conversation of this world (0: none of its chats).
fn chat_of(conv: &kourier_model::ConvKey) -> i64 {
    match conv {
        kourier_model::ConvKey::Direct(user) => CHATS
            .iter()
            .copied()
            .find(|c| *user == UserId::from_parts("tg", &c.to_string()).expect("id"))
            .unwrap_or(0),
        kourier_model::ConvKey::Group(group) if **group == *format!("tg:{SG}") => SG,
        _ => 0,
    }
}

/// The fake core's commit: the envelope's messages appended to the core log and synced.
fn commit(dir: &Path, events: &[BackendEvent]) {
    let mut lines = String::new();
    for e in events {
        match e {
            BackendEvent::Message(m) => {
                lines.push_str(&format!("{} {}\n", chat_of(&m.conv), m.local));
            }
            BackendEvent::HistoryBatch { conv, messages, .. } => {
                for m in messages {
                    lines.push_str(&format!("{} {}\n", chat_of(conv), m.local));
                }
            }
            // Never in this world: no gap reaches the cap.
            BackendEvent::Notice(kourier_backend_api::BackendNotice::HistoryLost { .. }) => {
                std::process::exit(9);
            }
            _ => {}
        }
    }
    if lines.is_empty() {
        return;
    }
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(CORE))
        .and_then(|mut f| {
            f.write_all(lines.as_bytes())?;
            f.sync_data()
        });
    if written.is_err() {
        std::process::exit(8);
    }
}

/// The wall bound of one [`pump`]: a guard against a hang, never a timing fact. The whole loop of
/// children takes 14-19 s of a quiet machine, so one wait needs a small part of it; the bound is
/// 60 s so that a starved child (a load of 100 and more, 4,000 tests side by side) still gets
/// through, and a child that hangs for good fails the test within a minute. No assert reads it: a
/// pump that runs out ends the child with its own exit code.
const PUMP_BOUND: Duration = Duration::from_secs(60);

/// Events (at most [`PUMP_BOUND`]) until `done` holds: every durable envelope committed to the core
/// log, then acked; the fake clock advanced by the sync lane's pace while idle, and (`release`)
/// every held reply released.
fn pump(run: &mut Run, dir: &Path, release: bool, mut done: impl FnMut(&Run) -> bool) -> bool {
    let by = StdInstant::now() + PUMP_BOUND;
    loop {
        if done(run) {
            return true;
        }
        if StdInstant::now() >= by {
            return false;
        }
        let Run {
            rt, ends, clock, ..
        } = run;
        let envelope = rt.block_on(async {
            tokio::select! {
                envelope = ends.events.recv() => Some(envelope),
                () = tokio::time::sleep(Duration::from_millis(20)) => None,
            }
        });
        match envelope {
            Some(Some(envelope)) => {
                if let Some(ack) = envelope.ack {
                    commit(dir, &envelope.events);
                    ack.complete(Ok(()));
                }
            }
            Some(None) => return false,
            None => {
                if release {
                    run.td.release();
                }
                clock.advance(Duration::from_millis(crate::session::lane::FETCH_PACE_MS));
            }
        }
    }
}

/// The first link of a [`Mode::WaitChat`] child: the phone login up to `linking`, where the actor
/// reads the objects that ride the `getMe` answer (script), then the core's ack of the
/// `AccountReady` links the account and the chat machine asks the chat it dropped past the bound.
/// Any failure panics (the child ends with a failed exit; the parent reports only that).
fn first_link(run: &mut Run) {
    let mut seen = Vec::new();
    let code = to_the_code_step(run, &mut seen);
    run.send(input(code.txn, &[(field::CODE, CODE)]));
    run.step(&mut seen, step::LINKING);
    let ack = run
        .until(&mut seen, |e| {
            matches!(e, BackendEvent::AccountReady { .. })
        })
        .expect("the AccountReady is not durable");
    ack.complete(Ok(()));
    run.step(&mut seen, step::DONE);
}

/// The child: reads its [`Mode`] and the account directory from its socket on fd 3, runs the
/// actor until the core holds the world, ends it with a Shutdown and exits 0. Does nothing in a
/// normal test run.
#[test]
fn kill_child() {
    if std::env::var_os(kourier_db::failpoint::ENV).is_none() {
        return;
    }
    let Some(mut sock) = kourier_tdlib::spawn::helper_socket() else {
        std::process::exit(2);
    };
    let mut text = String::new();
    if sock.read_to_string(&mut text).is_err() {
        std::process::exit(3);
    }
    let Some((mode, dir)) = text
        .split_once('\n')
        .and_then(|(m, d)| Some((Mode::parse(m)?, d)))
    else {
        std::process::exit(3);
    };
    let dir = PathBuf::from(dir);
    // What TDLib never processed comes live again (its difference), then the NEW messages.
    let done: BTreeSet<(i64, i64)> = processed(&dir).into_iter().collect();
    let mut live: Vec<(i64, i64)> = if mode.fresh() {
        // The fresh-link world's one live message, unless TDLib processed it before a kill.
        Some(FRESH_LIVE)
            .into_iter()
            .filter(|m| !done.contains(m))
            .collect()
    } else {
        stream().into_iter().filter(|m| !done.contains(m)).collect()
    };
    if mode.extra() {
        live.extend((PER_CHAT + 1..=PER_CHAT + EXTRA).flat_map(|id| CHATS.map(|c| (c, id))));
        live.extend(SG_EXTRA.map(|id| (SG, id)));
    }
    if let Mode::Redeliver(chat, id) = mode {
        live.push((chat, id));
    }
    let mut setup = Setup {
        at: Some(dir.clone()),
        ..Setup::login(script(
            &dir,
            mode,
            if mode == Mode::Early { &live } else { &[] },
        ))
    };
    if mode == Mode::WaitChat {
        // A first link: the login's own start mode, the chat machine's bound lowered.
        setup.timings = Timings {
            learn_bound: Some(WAIT_BOUND),
            ..short()
        };
    } else {
        setup.start = StartMode::Resume;
    }
    let mut run = open(setup);
    if mode == Mode::WaitChat {
        first_link(&mut run);
    } else {
        run.send(Command::Connect);
    }
    run.td.push_update(conn_update("connectionStateReady"));
    if matches!(
        mode,
        Mode::Early | Mode::Reconcile | Mode::WaitChat | Mode::FreshHeld | Mode::FreshEarly
    ) {
        // The held answer was asked: the live messages come before it.
        if !pump(&mut run, &dir, false, |r: &Run| r.td.held() > 0) {
            std::process::exit(7);
        }
        for (chat, id) in live.drain(..) {
            run.td.push_update(new_message(chat, id));
        }
        if mode == Mode::FreshHeld {
            // The live message must reach the actor ALONE: were it not held for its chat's plan, it
            // would be committed before the page comes back. Released at once, the page's answer
            // would join it in ONE group commit (the actor takes every ready frame before it
            // commits) and no kill could tell the two orders apart. A settle of idle pump ticks,
            // never an assert: a slow machine only makes the loop a weaker detector.
            let mut ticks = 0;
            pump(&mut run, &dir, false, |_: &Run| {
                ticks += 1;
                ticks > SETTLE_TICKS
            });
        }
        run.td.release();
    }
    let online = |r: &Run| matches!(*r.ends.conn.borrow(), ConnState::Online { .. });
    if !pump(&mut run, &dir, true, online) {
        std::process::exit(4);
    }
    // In two rounds, the first message of each chat, then the rest, each round in the core
    // before the next: a kill in the second round's commit leaves a chat's cursor SEVERAL
    // messages below what TDLib processed (a tail the reconcile pages, not only `td_last`).
    let rounds: Vec<Vec<(i64, i64)>> = if mode == Mode::Paced {
        // U7: one message per round, each in the core's log (hence committed) before the next
        // is pushed: the n-th commit with rows holds exactly the n-th message of the stream.
        live.into_iter().map(|m| vec![m]).collect()
    } else {
        let mut rounds: BTreeMap<i64, Vec<(i64, i64)>> = BTreeMap::new();
        for (chat, id) in live {
            rounds.entry(id.min(2)).or_default().push((chat, id));
        }
        rounds.into_values().collect()
    };
    for round in rounds {
        for &(chat, id) in &round {
            run.td.push_update(new_message(chat, id));
        }
        let held = |_: &Run| {
            let have = delivered(&dir);
            round
                .iter()
                .all(|&(c, id)| have.contains(&(c, (id * STEP).to_string())))
        };
        if !pump(&mut run, &dir, true, held) {
            std::process::exit(5);
        }
    }
    let want = want_of(mode);
    let whole = |_: &Run| delivered(&dir) == want;
    if !pump(&mut run, &dir, true, whole) {
        std::process::exit(5);
    }
    if run.end_with(Command::Shutdown) != Exit::Shutdown {
        std::process::exit(6);
    }
    std::process::exit(0);
}

/// The world as the core must hold it: (chat, local) of the stream, plus the [`EXTRA`] messages
/// when `extra`.
fn want(extra: bool) -> BTreeSet<(i64, String)> {
    let top = if extra { PER_CHAT + EXTRA } else { PER_CHAT };
    let mut world: BTreeSet<(i64, String)> = (1..=top)
        .flat_map(|id| CHATS.map(|c| (c, (id * STEP).to_string())))
        .collect();
    let sg = SG_STREAM.iter().chain(SG_EXTRA.iter().filter(|_| extra));
    world.extend(sg.map(|id| (SG, (id * STEP).to_string())));
    world
}

/// The fresh-link world as the core must hold it (TG-4 K7): per private chat the last message
/// and the unread messages below it (U5: exactly those, never a read one), and the live message.
fn want_fresh() -> BTreeSet<(i64, String)> {
    CHATS
        .iter()
        .flat_map(|&c| {
            fresh_batch(c)
                .into_iter()
                .map(move |id| (c, (id * STEP).to_string()))
        })
        .collect()
}

/// The server ids of `chat`'s whole fresh-link batch: the live message ([`FRESH_LIVE`]) when it is
/// the chat's, the last message and the unread ones below it (`unread - 1` below the last, U5:
/// the read messages [`FRESH_READ`] stay out of the core).
fn fresh_batch(chat: i64) -> BTreeSet<i64> {
    let mut ids: BTreeSet<i64> = FRESH_UNREAD.iter().copied().collect();
    if chat == FRESH_LIVE.0 {
        ids.insert(FRESH_LIVE.1);
    }
    ids
}

/// What the core must hold after a recovery in `mode`.
fn want_of(mode: Mode) -> BTreeSet<(i64, String)> {
    if mode.fresh() {
        want_fresh()
    } else {
        want(mode.extra())
    }
}

/// The core's state: the set of what it committed (its idempotent ingest).
fn delivered(dir: &Path) -> BTreeSet<(i64, String)> {
    committed(dir).into_iter().collect()
}

/// A linked account directory with an empty receive state.
fn account(dir: &Path) {
    let conn = db::open(&dir.join("backend.db"), &DbKey::from_bytes(KEY)).expect("open");
    db::create_account(
        &conn,
        &db::AccountRow {
            env: TgEnv::Production,
            user_id: Some(OWN_ID),
            pin: PIN.commit.to_owned(),
            created_ms: 1,
        },
    )
    .expect("row");
}

/// How one child run over `dir` ended: `true` = aborted at the failpoint, `false` = a normal
/// exit (any other end fails the test with the child's exit code only).
fn run_child(dir: &Path, spec: &str) -> bool {
    run_mode(dir, spec, Mode::Plain)
}

/// [`run_child`] in `mode`.
fn run_mode(dir: &Path, spec: &str, mode: Mode) -> bool {
    let mut s = kourier_tdlib::spawn::spawn_self(KILL_CHILD, (kourier_db::failpoint::ENV, spec))
        .expect("spawn the child");
    s.sock
        .write_all(format!("{}\n", mode.line()).as_bytes())
        .expect("send the mode");
    s.sock
        .write_all(dir.as_os_str().as_encoded_bytes())
        .expect("send the directory");
    s.sock
        .shutdown(std::net::Shutdown::Write)
        .expect("end the directory");
    // Keep the child's stderr open until it ends (its abort line goes there); never shown.
    let mut err = Vec::new();
    if let Some(mut e) = s.stderr.take() {
        let _ = e.read_to_end(&mut err);
    }
    let status = s.child.wait().expect("wait for the child");
    match status.signal() {
        Some(libc::SIGABRT) => true,
        Some(_) => panic!("the child died by another signal"),
        None => {
            assert!(
                status.success(),
                "the child failed (exit {:?}) at {spec}",
                status.code()
            );
            false
        }
    }
}

/// `run_state.clean_end` of `dir` (`None`: no row).
fn clean_end(dir: &Path) -> Option<i64> {
    let conn = db::open(&dir.join("backend.db"), &DbKey::from_bytes(KEY)).expect("reopen");
    conn.query_row("SELECT clean_end FROM run_state WHERE id = 1", [], |r| {
        r.get(0)
    })
    .optional()
    .expect("read run_state")
}

/// C10a: a child aborted after its first receive commit leaves `run_state.clean_end` 0 (the
/// next start knows the run did not end cleanly); the recovery child's clean stop sets it 1, and
/// the core then holds the whole stream.
#[test]
fn a_kill_leaves_clean_end_zero() {
    if !kourier_db::failpoint::ENABLED {
        eprintln!("SKIPPED: failpoints are off in this build");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    account(dir.path());
    assert!(
        run_child(dir.path(), &format!("{}:1", crate::store::AFTER_ROWS)),
        "the first commit was never reached"
    );
    assert_eq!(clean_end(dir.path()), Some(0), "a killed run looks clean");
    assert!(!run_child(dir.path(), UNARMED), "the recovery was killed");
    assert_eq!(clean_end(dir.path()), Some(1), "a clean stop left 0");
    assert_eq!(
        delivered(dir.path()),
        want(false),
        "the core does not hold the stream after the recovery"
    );
}

/// Each chat's cursor (`chat_state.delivered_max_id`, server ids; chats without a row absent).
fn cursors(dir: &Path) -> BTreeMap<i64, i64> {
    let conn = db::open(&dir.join("backend.db"), &DbKey::from_bytes(KEY)).expect("reopen");
    let mut stmt = conn
        .prepare("SELECT chat_id, delivered_max_id FROM chat_state")
        .expect("prepare chat_state");
    stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? / STEP))
    })
    .expect("read chat_state")
    .map(|r| r.expect("chat_state row"))
    .filter(|(_, max)| *max > 0)
    .collect()
}

/// The messages (server ids) each chat's rows hold: the waiting `core_outbox` rows and what the
/// core committed.
fn rows_by_chat(dir: &Path) -> BTreeMap<i64, BTreeSet<i64>> {
    let conn = db::open(&dir.join("backend.db"), &DbKey::from_bytes(KEY)).expect("reopen");
    let mut held: Vec<(i64, String)> = committed(dir);
    for row in kourier_db::outbox::peek(&conn, 10_000).expect("peek core_outbox") {
        let events = kourier_backend_api::decode_outbox_row(&row.envelope).expect("decode a row");
        for e in &events {
            match e {
                BackendEvent::Message(m) => held.push((chat_of(&m.conv), m.local.to_string())),
                BackendEvent::HistoryBatch { conv, messages, .. } => {
                    held.extend(
                        messages
                            .iter()
                            .map(|m| (chat_of(conv), m.local.to_string())),
                    );
                }
                _ => {}
            }
        }
    }
    let mut rows: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    for (chat, local) in held {
        let id = local.parse::<i64>().expect("a server id") / STEP;
        rows.entry(chat).or_default().insert(id);
    }
    rows
}

/// The newest message (server id) each chat's rows hold.
fn rows_max(dir: &Path) -> BTreeMap<i64, i64> {
    rows_by_chat(dir)
        .into_iter()
        .filter_map(|(chat, ids)| Some((chat, ids.last().copied()?)))
        .collect()
}

/// What a kill at `at` must leave: an unclean run, and every chat's cursor exactly at the newest
/// message its rows hold (never ahead: nothing lost; never behind: the cursor and its rows are
/// one transaction).
fn check_kill(dir: &Path, at: &str) {
    // Only a kill AFTER a commit may follow the clean end's own commit (the run did end clean,
    // nothing is left above a cursor); every other point is inside a transaction or before one.
    if at.starts_with(crate::store::AFTER_ROWS) && clean_end(dir) == Some(1) {
        assert!(
            !has_tail(dir),
            "a run killed at {at} with a tail looks clean"
        );
    } else {
        assert_ne!(clean_end(dir), Some(1), "a run killed at {at} looks clean");
    }
    assert_eq!(
        cursors(dir),
        rows_max(dir),
        "after a kill at {at} a cursor is not at its rows"
    );
}

/// What a kill must leave of the fresh-link world (TG-4 K7), per private chat: EITHER the cursor
/// is 0 and no row of the chat is in `core_outbox` or the core log (nothing of the chat was
/// committed: the next run loads it fresh again), OR the rows hold the chat's WHOLE batch
/// ([`fresh_batch`]: its last message, the page's rows and the live message held for it, in one
/// transaction) and nothing else. The cursor-equals-newest equation is [`check_kill`]'s. How many
/// chats the kill left committed (0..=3), and whether the live chat's committed batch carried
/// the live message.
fn check_fresh(dir: &Path, at: &str) -> (usize, bool) {
    let cursors = cursors(dir);
    let rows = rows_by_chat(dir);
    let mut committed = 0;
    let mut live_in_batch = false;
    for chat in CHATS {
        match (cursors.get(&chat), rows.get(&chat)) {
            (None, None) => {}
            (None, Some(_)) => {
                panic!("after a kill at {at} a fresh chat has rows but no cursor");
            }
            (Some(_), held) => {
                assert_eq!(
                    held,
                    Some(&fresh_batch(chat)),
                    "after a kill at {at} a fresh chat's cursor moved without its whole batch \
                     (its last message, the page's rows and the held live message)"
                );
                committed += 1;
                live_in_batch |= chat == FRESH_LIVE.0;
            }
        }
    }
    (committed, live_in_batch)
}

/// The recovery: one unarmed child in `mode`, then the whole world in the core and a clean end.
fn recover(dir: &Path, mode: Mode, at: &str) {
    assert!(!run_mode(dir, UNARMED, mode), "the recovery was killed");
    assert_eq!(
        clean_end(dir),
        Some(1),
        "the recovery after {at} left no clean end"
    );
    assert_eq!(
        delivered(dir),
        want_of(mode),
        "the core does not hold the world after a kill at {at}"
    );
}

/// Whether some chat holds a processed message above its cursor (a tail TDLib processed and
/// Kourier never committed).
fn has_tail(dir: &Path) -> bool {
    let cursors = cursors(dir);
    processed(dir)
        .iter()
        .any(|(chat, id)| *id > cursors.get(chat).copied().unwrap_or(0))
}

/// Whether some chat with a stored cursor holds at least two processed messages above it: the
/// next start's decision PAGES that chat with something to find (one message would come as
/// `td_last` alone and the page below it holds nothing of the gap's range; a chat with no cursor is
/// loaded like a fresh one instead). The precondition of the reconcile loops, and the one the
/// mutant that judges a page by the live cursor needs ([`killed_with_tail`] builds it).
fn has_paged_tail(dir: &Path) -> bool {
    let cursors = cursors(dir);
    let done = processed(dir);
    cursors
        .iter()
        .any(|(chat, c)| done.iter().filter(|(ch, id)| ch == chat && id > c).count() >= 2)
}

/// Whether the supergroup holds a stored cursor and TDLib processed a message of it above that
/// cursor: a tail the start decision would find, but not once a live message of the supergroup
/// moved the cursor up to `td_last` before the decision ([`Mode::Early`]). A supergroup has no
/// B1 check gap (that is the private chats' and basic groups' rule), so only the id trigger's
/// gap fetches it. ONE processed message above the cursor is enough: a processed id is a real
/// message (the world's deleted id 3 is never processed), and a live id after the tail is above
/// every processed one (the stream is processed in order), hence more than one above the cursor.
/// (Unlike [`has_paged_tail`], which needs two. A PACED child's kill inside the commit of a
/// supergroup message after its first leaves exactly this state; [`run_ahead`] gives it to any
/// other kill after the supergroup's first message.)
fn has_sg_tail(dir: &Path) -> bool {
    let Some(&cursor) = cursors(dir).get(&SG) else {
        return false;
    };
    processed(dir)
        .iter()
        .any(|&(chat, id)| chat == SG && id > cursor)
}

/// Whether TDLib processed a message of the supergroup but the store holds nothing of it (no
/// cursor, hence no row): what a kill leaves while the supergroup's messages WAIT for its
/// `getChat` ([`Mode::WaitChat`]). The next start finds the supergroup with a stored cursor of 0:
/// its fresh-link load (last + unread page) is the only way back.
fn has_sg_waited(dir: &Path) -> bool {
    !cursors(dir).contains_key(&SG) && processed(dir).iter().any(|&(chat, _)| chat == SG)
}

/// The server ids of `chat`'s stream, in order.
fn chat_stream(chat: i64) -> Vec<i64> {
    if chat == SG {
        SG_STREAM.to_vec()
    } else {
        (1..=PER_CHAT).collect()
    }
}

/// The messages (server ids) TDLib processed in `chat` above the chat's stored cursor (a chat with
/// no cursor: all of them).
fn tail_of(dir: &Path, chat: i64) -> Vec<i64> {
    let cursor = cursors(dir).get(&chat).copied().unwrap_or(0);
    processed(dir)
        .into_iter()
        .filter(|&(c, id)| c == chat && id > cursor)
        .map(|(_, id)| id)
        .collect()
}

/// TDLib running AHEAD of the killed child (U7): appends the next messages of `chat`'s stream to
/// `dir`'s processed log, in the form a push logs them, until `want` processed messages of the chat
/// lie above its stored cursor. TDLib saves what it processed whatever Kourier's actor took before
/// the kill, so a longer tail is a state a real kill leaves; the log of a PACED child holds one
/// message above a cursor, and this states the rest instead of leaving it to how fast the link
/// thread logged the pushes. `false`, with nothing appended, when the stream has no more messages.
fn run_ahead(dir: &Path, chat: i64, want: usize) -> bool {
    let have = tail_of(dir, chat).len();
    if have >= want {
        return true;
    }
    let newest = processed(dir)
        .iter()
        .filter(|(c, _)| *c == chat)
        .map(|(_, id)| *id)
        .max()
        .unwrap_or(0);
    let next: Vec<i64> = chat_stream(chat)
        .into_iter()
        .filter(|&id| id > newest)
        .take(want - have)
        .collect();
    if next.len() < want - have {
        return false;
    }
    let path = dir.join(PROCESSED);
    let mut text = String::new();
    // A kill between a line and its newline leaves the last line without one: end it first, or
    // the first appended line would be glued to it and lost.
    if std::fs::read(&path).is_ok_and(|bytes| bytes.last().is_some_and(|&b| b != b'\n')) {
        text.push('\n');
    }
    for id in next {
        text.push_str(&new_message(chat, id).to_string());
        text.push('\n');
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| {
            f.write_all(text.as_bytes())?;
            f.sync_data()
        })
        .expect("extend the processed log");
    true
}

/// A directory a PACED first child ([`Mode::Paced`]) left killed inside the n-th receive commit
/// (n = 1, 2, ..) with a PAGED TAIL ([`has_paged_tail`]): some private chat has a stored cursor and
/// TDLib's log holds two of its messages above it. Certain by construction: the child pushes one
/// message at a time and the next only after the previous one is in the core's log, so the commit
/// of every message after a chat's first holds that message alone (processed and not committed,
/// one above the chat's cursor, whatever the machine's speed) and [`run_ahead`] states the second.
/// What the start commits before the first message do to `n` does not matter: the loop tries the
/// next n. Panics when no kill fits (a broken world, never a slow machine).
fn killed_with_tail() -> tempfile::TempDir {
    for n in 1..=MAX_HITS {
        let dir = tempfile::tempdir().expect("tempdir");
        account(dir.path());
        let at = format!("{}:{n}", crate::store::INSIDE_ROWS);
        if !run_mode(dir.path(), &at, Mode::Paced) {
            break;
        }
        let chat = CHATS
            .into_iter()
            .find(|&c| cursors(dir.path()).contains_key(&c) && !tail_of(dir.path(), c).is_empty());
        if let Some(chat) = chat
            && run_ahead(dir.path(), chat, 2)
        {
            assert!(
                has_paged_tail(dir.path()),
                "the kill at {at} left no paged tail after the log was extended"
            );
            // Counts only (the report's kill).
            eprintln!("paced kill with a paged tail: {at}");
            return dir;
        }
    }
    panic!("no kill at tg-inside-rows left the state the test needs");
}

/// A directory a first child left killed at `point`, where `fits` holds (the n-th hit tried
/// from 1 up). Only for a `fits` every kill after the first commit with rows satisfies (a kill
/// after a commit leaves a cursor), never for a state that depends on how commits split: that is
/// [`killed_with_tail`]'s.
fn killed_where(point: &str, fits: impl Fn(&Path) -> bool) -> tempfile::TempDir {
    for n in 1..=MAX_HITS {
        let dir = tempfile::tempdir().expect("tempdir");
        account(dir.path());
        if !run_child(dir.path(), &format!("{point}:{n}")) {
            break;
        }
        if fits(dir.path()) {
            return dir;
        }
    }
    panic!("no kill at {point} left the state the test needs");
}

/// Copies the account directory `from` (closed: its child ended) into a new one.
fn copy_of(from: &Path) -> tempfile::TempDir {
    let to = tempfile::tempdir().expect("tempdir");
    for entry in std::fs::read_dir(from).expect("read the directory") {
        let entry = entry.expect("entry");
        if entry.file_type().expect("type").is_file() {
            std::fs::copy(entry.path(), to.path().join(entry.file_name())).expect("copy");
        }
    }
    to
}

/// One failpoint's loop (brief §4.7): a child killed at the n-th hit of `point` (n = 1, 2, .. until
/// a child exits normally) over a copy of `from` (`None`: a new account), then an unarmed
/// recovery. After the kill the run is unclean (or ended clean with nothing above a cursor) and
/// every cursor is at its rows; after the recovery the core holds every message of the stream
/// exactly once (its set) and the run ended clean. How many kills it made.
fn kill_loop(point: &str, from: Option<&Path>) -> usize {
    let mut kills = 0;
    for n in 1..=MAX_HITS {
        let at = format!("{point}:{n}");
        let dir = match from {
            Some(from) => copy_of(from),
            None => {
                let dir = tempfile::tempdir().expect("tempdir");
                account(dir.path());
                dir
            }
        };
        if !run_child(dir.path(), &at) {
            assert_eq!(
                delivered(dir.path()),
                want(false),
                "the unkilled run at {at}"
            );
            break;
        }
        assert!(n < MAX_HITS, "{point} is still hit after {MAX_HITS} kills");
        kills += 1;
        check_kill(dir.path(), &at);
        recover(dir.path(), Mode::Plain, &at);
    }
    assert!(kills > 0, "{point} was never hit");
    // Counts only (the report's hits per point).
    eprintln!("kill loop {point}: {kills} kills");
    kills
}

/// C10b (brief §4.7): `every_processed_message_once_after_a_kill_at_every_point`, one test per
/// failpoint (they run side by side). `tg-in-reconcile` is hit only by a child that pages: it is
/// looped over copies of a directory a commit kill left with a tail above a cursor.
mod every_processed_message_once_after_a_kill_at_every_point {
    use super::*;

    fn armed() -> bool {
        if !kourier_db::failpoint::ENABLED {
            eprintln!("SKIPPED: failpoints are off in this build");
        }
        kourier_db::failpoint::ENABLED
    }

    #[test]
    fn tg_inside_rows() {
        if armed() {
            kill_loop(crate::store::INSIDE_ROWS, None);
        }
    }

    #[test]
    fn tg_after_rows() {
        if armed() {
            kill_loop(crate::store::AFTER_ROWS, None);
        }
    }

    #[test]
    fn tg_before_ship() {
        if armed() {
            kill_loop(crate::outbox::BEFORE_SHIP, None);
        }
    }

    #[test]
    fn tg_after_ack() {
        if armed() {
            kill_loop(crate::outbox::AFTER_ACK, None);
        }
    }

    #[test]
    fn tg_inside_clean_end() {
        if armed() {
            kill_loop(crate::store::INSIDE_CLEAN_END, None);
        }
    }

    #[test]
    fn tg_in_reconcile() {
        if armed() {
            let tail = killed_with_tail();
            kill_loop(crate::sync::reconcile::IN_RECONCILE, Some(tail.path()));
        }
    }
}

/// C10b (critic B1 rule 4): a kill leaves a tail TDLib processed and Kourier never committed; the
/// recovery's live messages (the rest of the stream and a NEW one per chat) arrive before
/// `loadChats` answers, so `td_last` is no longer above the cursor when the lists are loaded. The
/// first live message's gap fetches the tail: the core holds every message. The kill is a PACED
/// child's with a paged tail ([`killed_with_tail`]).
#[test]
fn live_messages_before_load_chats_after_a_kill_open_a_gap() {
    if !kourier_db::failpoint::ENABLED {
        eprintln!("SKIPPED: failpoints are off in this build");
        return;
    }
    let dir = killed_with_tail();
    recover(dir.path(), Mode::Early, "a tail");
}

/// TG-4 K6a (brief W4 (ii), critic M8): the supergroup's id trigger under SIGKILL. Every child
/// killed inside the n-th receive commit (n = 1, 2, .. until a child exits normally) is checked
/// like the per-point loops' kills; each one that leaves the supergroup a tail above its stored
/// cursor ([`has_sg_tail`]) is recovered in [`Mode::Early`]: the live supergroup message
/// (`SG_EXTRA`, and any unprocessed part of its stream) comes before the held `loadChats` answer,
/// so when the lists load `td_last` equals the cursor and the start decision opens nothing. The
/// only gap left is the id trigger's (`cursor < id - 1`, `GapCause::IdJump`): the reconcile
/// pages the tail from TDLib's history and the core holds the whole world. Without the trigger
/// the tail stays lost (the recovery's whole-stream check fails). The loop is over PRECONDITIONS
/// (a kill whose state fits), never over a clock: no kill that fits is a broken world. U7: the
/// children are PACED ([`Mode::Paced`]: one message per commit) and TDLib runs ahead of the
/// killed child by the supergroup's next message ([`run_ahead`]), so EVERY kill after the
/// supergroup's first message has a tail and `tails > 0` cannot depend on how a commit split; one
/// recovery per cursor value (the same cursor over the same log recovers the same way).
#[test]
fn every_supergroup_tail_after_a_kill_comes_back_through_the_id_trigger() {
    if !kourier_db::failpoint::ENABLED {
        eprintln!("SKIPPED: failpoints are off in this build");
        return;
    }
    let (mut kills, mut tails, mut recovered) = (0, 0, BTreeSet::new());
    for n in 1..=MAX_HITS {
        let at = format!("{}:{n}", crate::store::INSIDE_ROWS);
        let dir = tempfile::tempdir().expect("tempdir");
        account(dir.path());
        if !run_mode(dir.path(), &at, Mode::Paced) {
            assert_eq!(
                delivered(dir.path()),
                want(false),
                "the unkilled run at {at}"
            );
            break;
        }
        assert!(
            n < MAX_HITS,
            "tg-inside-rows is still hit after {MAX_HITS} kills"
        );
        kills += 1;
        check_kill(dir.path(), &at);
        // The paced child leaves ONE message above a cursor, and only in the commit of that
        // message; every other kill after the supergroup's first message left TDLib's log without
        // a message above its cursor. TDLib runs AHEAD (U7, `run_ahead`): it processed the next
        // message of the supergroup's stream whatever the killed child took. So a kill leaves the
        // supergroup a tail whenever it has a cursor and a message left to process, certain by
        // construction and never a matter of how a commit split.
        let cursor = cursors(dir.path()).get(&SG).copied();
        if cursor.is_some() {
            run_ahead(dir.path(), SG, 1);
        }
        if has_sg_tail(dir.path()) {
            tails += 1;
            // One recovery per state: the same cursor over the same log gives the same recovery.
            if recovered.insert(cursor) {
                recover(dir.path(), Mode::Early, &at);
            }
        }
    }
    assert!(
        tails > 0,
        "no kill inside a commit left the supergroup a tail above its cursor"
    );
    // Counts only (the report's kills and recoveries).
    eprintln!(
        "supergroup tail loop: {kills} kills, {tails} with a tail, {} recovered",
        recovered.len()
    );
}

/// TG-4 K6b (brief W4 (iii), critic M8): the waiting path under SIGKILL. Every [`Mode::WaitChat`]
/// child (a first link whose chat machine drops the supergroup and its chat past the bound, asks
/// `getChat` at the link and gets the answer only after the whole stream was pushed) is killed at
/// the n-th hit of `tg-inside-rows` and of `tg-after-rows` (n = 1, 2, .. until a child exits
/// normally) over a NEW account the child links itself. After every kill the cursors are at their
/// rows; the recovery (an unarmed, resumed child) must leave the whole world in the core, the
/// supergroup's messages too: where the kill came while they waited ([`has_sg_waited`]) the
/// supergroup has no stored cursor and TDLib's history is the only source, so the recovery is its
/// fresh-link load. The unkilled run at the end of each loop holds the stream, which only the
/// waiting messages' replay (`Messages::on_chat`) can give the supergroup. The loop is over
/// PRECONDITIONS (a kill whose state fits), never over a clock.
#[test]
fn every_chat_that_waited_for_its_getchat_after_a_kill_comes_back_through_the_fresh_load() {
    if !kourier_db::failpoint::ENABLED {
        eprintln!("SKIPPED: failpoints are off in this build");
        return;
    }
    let (mut kills, mut waited) = (0, 0);
    for point in [crate::store::INSIDE_ROWS, crate::store::AFTER_ROWS] {
        for n in 1..=MAX_HITS {
            let at = format!("{point}:{n}");
            // No account row: the child's login makes it.
            let dir = tempfile::tempdir().expect("tempdir");
            if !run_mode(dir.path(), &at, Mode::WaitChat) {
                assert_eq!(
                    delivered(dir.path()),
                    want(false),
                    "the unkilled run at {at}"
                );
                break;
            }
            assert!(n < MAX_HITS, "{point} is still hit after {MAX_HITS} kills");
            kills += 1;
            check_kill(dir.path(), &at);
            if has_sg_waited(dir.path()) {
                waited += 1;
            }
            recover(dir.path(), Mode::Plain, &at);
        }
    }
    assert!(
        waited > 0,
        "no kill came while the supergroup's messages waited for their getChat"
    );
    // Counts only (the report's kills and recoveries).
    eprintln!("waiting chat loop: {kills} kills, {waited} with the supergroup waiting");
}

/// C10b (critic B1, the second variant): the recovery's reconcile pages a kill's tail; live
/// messages (the rest of the stream, a NEW one per chat) arrive while its first page is held and
/// move the cursor above the range. The page is judged by its snapshot range: nothing is lost.
#[test]
fn a_live_message_during_a_reconcile_after_a_kill_loses_nothing() {
    if !kourier_db::failpoint::ENABLED {
        eprintln!("SKIPPED: failpoints are off in this build");
        return;
    }
    let dir = killed_with_tail();
    recover(dir.path(), Mode::Reconcile, "a tail");
}

/// C10b (RC3, synthetic: no recording holds a re-delivery): after a kill, TDLib's difference
/// re-delivers a message Kourier had committed. It reaches the core under the same local (its
/// idempotent ingest keeps ONE message).
#[test]
fn a_redelivery_through_the_difference_is_one_row() {
    if !kourier_db::failpoint::ENABLED {
        eprintln!("SKIPPED: failpoints are off in this build");
        return;
    }
    let dir = killed_where(crate::store::AFTER_ROWS, |d| !cursors(d).is_empty());
    let (chat, id) = cursors(dir.path())
        .into_iter()
        .next()
        .expect("a committed chat");
    // `recover` asserts the core's SET equals the stream: a second local for the same message
    // would be a second message there. How many times the same row shipped is not asserted (a
    // kill before an ack's delete ships a row again, legitimately).
    recover(dir.path(), Mode::Redeliver(chat, id), "a commit");
}

/// TG-4 K7 (brief section 4, critic H1 (c)): one failpoint's loop over the fresh-link world. A
/// [`Mode::FreshHeld`] / [`Mode::FreshEarly`] child (`first`) is killed at the n-th hit of `point`
/// (n = 1, 2, .. until a child exits normally) over a NEW account whose receive state is empty;
/// after every kill the cursor equation ([`check_kill`]) and the batch rule ([`check_fresh`])
/// hold, and the recovery ([`Mode::FreshPlain`]) leaves exactly the world of [`want_fresh`] in the
/// core and a clean end. The loop is over PRECONDITIONS (the n-th hit exists; `batches > 0`: some
/// kill left a chat committed, so the batch rule's second half ran), never over a clock.
/// How many kills it made.
fn fresh_loop(point: &str, first: Mode) -> usize {
    let (mut kills, mut batches, mut live_batches, mut empty) = (0, 0, 0, 0);
    let mut repeats = 0;
    for n in 1..=MAX_HITS {
        let at = format!("{point}:{n}");
        let dir = tempfile::tempdir().expect("tempdir");
        account(dir.path());
        if !run_mode(dir.path(), &at, first) {
            assert_eq!(
                delivered(dir.path()),
                want_fresh(),
                "the unkilled run at {at}"
            );
            // "Once each" in a run that was never killed, in BOTH variants (U4): the replay of
            // the held live rows skips what the batch delivered, so even in the early-live
            // variant, where TDLib's told last message IS the live message, no row reaches the
            // core twice.
            repeats = committed(dir.path()).len() - delivered(dir.path()).len();
            assert_eq!(repeats, 0, "the unkilled run at {at} delivered a row twice");
            break;
        }
        assert!(n < MAX_HITS, "{point} is still hit after {MAX_HITS} kills");
        kills += 1;
        check_kill(dir.path(), &at);
        let (committed, live_in_batch) = check_fresh(dir.path(), &at);
        batches += usize::from(committed > 0);
        live_batches += usize::from(live_in_batch);
        empty += usize::from(committed == 0);
        recover(dir.path(), Mode::FreshPlain, &at);
    }
    assert!(kills > 0, "{point} was never hit");
    assert!(
        batches > 0,
        "no kill at {point} left a fresh chat committed: the batch rule never ran"
    );
    // Counts only (the report's kills and what they left).
    eprintln!(
        "fresh loop {point} ({}): {kills} kills, {batches} with a batch committed, {live_batches} with the live chat's, {empty} with none, {repeats} repeated rows in the unkilled run",
        first.line()
    );
    kills
}

/// TG-4 K7: the fresh-link world's kill loops with the live message pushed while its chat's first
/// page is held (the plan is open), one test per failpoint (they run side by side). A kill at any
/// point leaves each chat either with no cursor and no row, or with its whole batch (the last
/// message, the page's rows and the held live message) and its cursor at the newest of them; the
/// recovery gives the core exactly the world. (Mutants: K5's early last message back; the live
/// hold skipped for a chat with an open plan.)
mod every_fresh_chat_with_its_unread_page_after_a_kill_at_every_point {
    use super::*;

    fn armed() -> bool {
        if !kourier_db::failpoint::ENABLED {
            eprintln!("SKIPPED: failpoints are off in this build");
        }
        kourier_db::failpoint::ENABLED
    }

    #[test]
    fn tg_inside_rows() {
        if armed() {
            fresh_loop(crate::store::INSIDE_ROWS, Mode::FreshHeld);
        }
    }

    #[test]
    fn tg_after_rows() {
        if armed() {
            fresh_loop(crate::store::AFTER_ROWS, Mode::FreshHeld);
        }
    }

    #[test]
    fn tg_before_ship() {
        if armed() {
            fresh_loop(crate::outbox::BEFORE_SHIP, Mode::FreshHeld);
        }
    }

    #[test]
    fn tg_after_ack() {
        if armed() {
            fresh_loop(crate::outbox::AFTER_ACK, Mode::FreshHeld);
        }
    }
}

/// TG-4 K7, critic H1 (c): the early-live variant. The live message comes BEFORE the main list's
/// `loadChats` answer (its chat is not known yet: the message waits for it, then for its batch),
/// and TDLib's answer tells that chat with the message as its last. The same per-kill and
/// recovery checks as `every_fresh_chat_with_its_unread_page_after_a_kill_at_every_point`. (Mutant:
/// the hold only while a plan is open, the first design: the message lands before the lists load.)
mod a_live_message_before_the_lists_load_after_a_kill_at_every_point {
    use super::*;

    fn armed() -> bool {
        if !kourier_db::failpoint::ENABLED {
            eprintln!("SKIPPED: failpoints are off in this build");
        }
        kourier_db::failpoint::ENABLED
    }

    #[test]
    fn tg_inside_rows() {
        if armed() {
            fresh_loop(crate::store::INSIDE_ROWS, Mode::FreshEarly);
        }
    }

    #[test]
    fn tg_after_rows() {
        if armed() {
            fresh_loop(crate::store::AFTER_ROWS, Mode::FreshEarly);
        }
    }

    #[test]
    fn tg_before_ship() {
        if armed() {
            fresh_loop(crate::outbox::BEFORE_SHIP, Mode::FreshEarly);
        }
    }

    #[test]
    fn tg_after_ack() {
        if armed() {
            fresh_loop(crate::outbox::AFTER_ACK, Mode::FreshEarly);
        }
    }
}
