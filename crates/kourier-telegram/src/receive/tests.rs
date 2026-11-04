//! The receive machines (TG-3 C5a..): replayed from TDLib's recordings and the scrubbed login
//! fixture by name, synthetic lines only where no recording holds the case (L-14). Nothing here
//! prints a recorded value: assertions name ids and kinds.

use kourier_backend_api::BackendEvent;
use kourier_model::{ConvKey, Patch};
use serde_json::Value;

use super::chats::{self, Chats, LEARN_BOUND};
use super::*;
use crate::replay::{Item, Part, Recording};
use crate::store::{ChatKind, Gate, Held};
use crate::tdapi::{self, Update};

/// The account's own id in the fixture and the recordings.
const ME: i64 = 100;

fn testdata(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name);
    std::fs::read_to_string(path).expect("a file of testdata/")
}

/// Hands one TDLib line to the chat machine (a line that is no receive update gives nothing).
fn feed(chats: &mut Chats, line: &str) -> Out {
    match tdapi::parse_update(line) {
        Some(Update::Receive(r)) => chats.on_update(&r),
        _ => Out::default(),
    }
}

/// Every object TDLib wrote in `parts`, in the recording's order (the updates and the objects of
/// each recorded request's reply alike), and how many recorded answers that took (Q-L11).
fn objects<'a>(parts: impl IntoIterator<Item = &'a Part>) -> (Vec<String>, usize) {
    let mut lines = Vec::new();
    let mut answers = 0;
    for part in parts {
        for item in &part.items {
            match item {
                Item::Update(v) => lines.push(v.to_string()),
                Item::Request(r) => {
                    answers += 1;
                    lines.extend(r.reply.iter().map(Value::to_string));
                }
            }
        }
    }
    (lines, answers)
}

fn feed_all(chats: &mut Chats, lines: &[String]) -> Out {
    let mut out = Out::default();
    for l in lines {
        out.extend(feed(chats, l));
    }
    out.extend(chats.flush());
    out
}

/// The conversation patches of `out`, as (key, title, gate).
fn upserts(out: &Out) -> Vec<(ConvKey, Option<String>, Option<Gate>)> {
    out.staged
        .rows
        .iter()
        .flat_map(|r| &r.events)
        .filter_map(|e| match &e.event {
            BackendEvent::ConversationUpsert(p) => Some((
                p.key.clone(),
                match &p.title {
                    Patch::Set(t) => Some(t.clone()),
                    Patch::Clear | Patch::Unchanged => None,
                },
                e.gate,
            )),
            _ => None,
        })
        .collect()
}

fn direct(user: i64) -> ConvKey {
    ConvKey::parse(&format!("direct:tg:{user}")).expect("a direct key")
}

fn group(chat: i64) -> ConvKey {
    ConvKey::parse(&format!("group:tg:{chat}")).expect("a group key")
}

/// The features twin (KT-1b): `kourier-fake` states them for the chats of the fake that plays Telegram
/// (`kourier_fake::telegram::telegram_features`, with no import of this crate), and this is the
/// backend's own statement: equal for a direct chat and a group. Mutant: edits taken only within a
/// window here.
#[test]
fn the_fake_playing_telegram_states_the_features_this_backend_states() {
    let stated = chats::features();
    for group in [false, true] {
        assert_eq!(
            kourier_fake::telegram::telegram_features(group),
            stated,
            "group {group}"
        );
    }
    // What the statement says (TG-3): edits taken with no window, received deletes with none,
    // receipts and typing always on, the history served on demand, no send side yet.
    assert!(
        stated
            .edit_receive
            .is_some_and(|w| w.max_age_secs.is_none())
    );
    assert!(stated.delete_receive.is_none());
    assert!(stated.read_receipts && stated.typing);
    assert_eq!(stated.history, kourier_model::HistorySource::OnDemand);
}

/// Critic H1, from the scrubbed login fixture (`login-flow.jsonl` lines 150-173: Ready, then
/// `updateUser` 4242, its `updateNewChat` with no position, `updateChatAddedToList` main): nothing
/// is staged before the link; the link states chat 4242 as a listed conversation with its title.
#[test]
fn chats_told_before_the_link_are_listed_after_the_ack() {
    let text = testdata("login-flow.jsonl");
    let lines: Vec<&str> = text.lines().skip(149).take(24).collect();
    assert!(
        lines[0].contains("authorizationStateReady"),
        "line 150 is Ready"
    );
    let mut chats = Chats::new(None);
    for l in &lines {
        assert!(feed(&mut chats, l).is_empty(), "staged before the link");
    }
    assert!(
        chats.knows(4242),
        "the chat told before the link was not learnt"
    );
    let out = chats.link(ME);
    assert_eq!(
        upserts(&out),
        [(
            direct(4242),
            Some("Bo Peer".to_owned()),
            Some(Gate::Chat(4242))
        )]
    );
    let facts = out
        .staged
        .chats
        .iter()
        .find(|f| f.chat_id == 4242)
        .expect("the chat's facts");
    assert_eq!(
        (facts.kind, facts.listed, facts.held),
        (Some(ChatKind::Direct), Some(true), Some(None))
    );
    assert!(out.fetch.is_empty());
    let printed = format!("{out:?}");
    assert!(!printed.contains("Bo Peer"), "Debug printed a title");
}

/// L-13: a chat TDLib only KNOWS (no position, not added to a list, no message) is kept as a fact
/// and is no conversation; a main-list position lists it. Synthetic (no recording tells a
/// known-only chat).
#[test]
fn a_known_only_chat_is_not_listed() {
    let mut chats = Chats::new(Some(ME));
    assert!(upserts(&chats.link(ME)).is_empty());
    feed(
        &mut chats,
        r#"{"@type":"updateUser","user":{"@type":"user","id":207,"first_name":"Fay","last_name":"Sample","type":{"@type":"userTypeRegular"}}}"#,
    );
    let mut told = feed(
        &mut chats,
        r#"{"@type":"updateNewChat","chat":{"@type":"chat","id":207,"type":{"@type":"chatTypePrivate","user_id":207},"title":"Fay Sample","positions":[]}}"#,
    );
    told.extend(chats.flush());
    assert!(upserts(&told).is_empty(), "a known-only chat was listed");
    assert_eq!(told.staged.chats.len(), 1);
    assert_eq!(told.staged.chats[0].listed, Some(false));
    let mut placed = feed(
        &mut chats,
        r#"{"@type":"updateChatPosition","chat_id":207,"position":{"@type":"chatPosition","list":{"@type":"chatListMain"},"order":"7301444000000000007","is_pinned":false}}"#,
    );
    placed.extend(chats.flush());
    assert_eq!(
        upserts(&placed).len(),
        1,
        "a main-list position did not list it"
    );
    // A delivered message lists a chat too (C6a calls this).
    feed(
        &mut chats,
        r#"{"@type":"updateNewChat","chat":{"@type":"chat","id":-301,"type":{"@type":"chatTypeBasicGroup","basic_group_id":301},"title":"Group 7","positions":[]}}"#,
    );
    let delivered = chats.on_delivered(-301, 5 << 20, false);
    assert!(
        upserts(&delivered).is_empty(),
        "the patch rides the message's row"
    );
    assert_eq!(delivered.staged.chats[0].listed, Some(true));
    assert_eq!(
        chats.row_upsert(-301).map(|e| e.gate),
        Some(Some(Gate::Restate(-301)))
    );
    assert!(upserts(&chats.flush()).is_empty(), "stated twice");
}

/// H5, L4 (his TG4): a broadcast channel (403) and a public supergroup (402, an active username)
/// of `rec-basic-group-upgrade.jsonl` are held, its plain supergroup (401), basic group and private
/// chats are conversations; a bot's chat (`receive-kinds.jsonl` 205) and a channel's
/// direct-messages group (synthetic chat of its supergroup 405) are held too.
#[test]
fn channels_bots_and_public_groups_are_held() {
    let rec = Recording::load("rec-basic-group-upgrade.jsonl");
    let (lines, answers) = objects(rec.parts.iter().take(1));
    assert_eq!(answers, 16, "recorded answers used");
    let mut chats = Chats::new(Some(ME));
    chats.link(ME);
    let out = feed_all(&mut chats, &lines);
    for (chat, held) in [
        (-1_000_000_000_403, Some(Held::Channel)),
        (-1_000_000_000_402, Some(Held::Public)),
        (-1_000_000_000_401, None),
        (-301, None),
        (201, None),
        (202, None),
        (203, None),
    ] {
        assert_eq!(chats.held(chat), held, "chat {chat}");
        assert_eq!(chats.is_mapped(chat), held.is_none(), "chat {chat} mapped");
    }
    let keys: Vec<ConvKey> = upserts(&out).into_iter().map(|u| u.0).collect();
    let channel = ConvKey::parse("channel:tg:-1000000000403").expect("a channel key");
    assert!(!keys.contains(&channel), "the held channel was stated");
    assert!(
        !keys.contains(&group(-1_000_000_000_402)),
        "the held public group was stated"
    );
    assert!(keys.contains(&group(-1_000_000_000_401)));
    assert!(keys.contains(&direct(203)), "the archive's chat");
    let held_fact = out
        .staged
        .chats
        .iter()
        .rfind(|f| f.chat_id == -1_000_000_000_403)
        .expect("the channel's facts");
    assert_eq!(
        (held_fact.kind, held_fact.held),
        (Some(ChatKind::Channel), Some(Some(Held::Channel)))
    );

    let kinds = testdata("receive-kinds.jsonl");
    for l in kinds.lines().take(8) {
        feed(&mut chats, l);
    }
    assert_eq!(chats.held(205), Some(Held::Bot), "a bot's chat");
    feed(
        &mut chats,
        r#"{"@type":"updateNewChat","chat":{"@type":"chat","id":-1000000000405,"type":{"@type":"chatTypeSupergroup","supergroup_id":405,"is_channel":false},"title":"Group 9","positions":[{"@type":"chatPosition","list":{"@type":"chatListMain"},"order":"7301444000000000009","is_pinned":false}]}}"#,
    );
    assert_eq!(chats.held(-1_000_000_000_405), Some(Held::DirectMessages));
    assert_eq!(chats.held(-1_000_000_000_406), Some(Held::Channel));
    assert!(chats.conv(-1_000_000_000_406).is_none());
}

/// RC11 (`rec-basic-group-upgrade.jsonl`, the whole recording): the upgrade records the
/// supergroup's chat as the basic group's `moved_to`; the new supergroup is a listed conversation.
#[test]
fn the_upgrade_records_moved_to() {
    let rec = Recording::load("rec-basic-group-upgrade.jsonl");
    let (lines, answers) = objects(&rec.parts);
    assert_eq!(answers, 17, "recorded answers used");
    let mut chats = Chats::new(Some(ME));
    chats.link(ME);
    let out = feed_all(&mut chats, &lines);
    let moved = out
        .staged
        .chats
        .iter()
        .rfind(|f| f.chat_id == -301)
        .expect("the basic group's facts");
    assert_eq!(moved.moved_to, Some(-1_000_000_000_404));
    assert!(chats.is_mapped(-1_000_000_000_404));
    assert!(
        upserts(&out)
            .iter()
            .any(|u| u.0 == group(-1_000_000_000_404))
    );
    assert_eq!(chats.kind(-1_000_000_000_404), Some(ChatKind::Supergroup));
}

/// `rec-gap-unknown-sender.jsonl` step `push-gap`: the difference tells `updateUser` and
/// `updateNewChat` before the message, so the chat is known when its message comes (no `getChat`);
/// a chat never told is asked once.
#[test]
fn a_chat_learnt_from_a_difference_is_known() {
    let rec = Recording::load("rec-gap-unknown-sender.jsonl");
    let (lines, answers) = objects([rec.part("push-gap")]);
    assert_eq!(answers, 0, "the part holds no request");
    let mut chats = Chats::new(Some(ME));
    chats.link(ME);
    let first_message = lines
        .iter()
        .position(|l| l.contains("\"updateNewMessage\""))
        .expect("a message in the part");
    feed_all(&mut chats, &lines[..first_message]);
    assert!(chats.knows(201), "the chat told by the difference");
    assert_eq!(chats.kind(201), Some(ChatKind::Direct));
    assert_eq!(chats.conv(201), Some(direct(201)));
    assert!(
        chats.reference(201).fetch.is_empty(),
        "a known chat was asked"
    );
    assert_eq!(chats.reference(999).fetch, [Fetch::Chat(999)]);
    assert!(chats.reference(999).fetch.is_empty(), "asked twice");
    assert_eq!(
        Fetch::Chat(999).request(),
        serde_json::json!({"@type": "getChat", "chat_id": 999})
    );
}

/// TF4 (review dF4): a supergroup chat whose supergroup object the machine never learnt (dropped by
/// the pre-link bound; TDLib never tells it twice) is undecided; `getSupergroup` is asked ONCE and
/// its answer decides the chat. (Mutant: no `getSupergroup`, the chat stays undecided.)
#[test]
fn a_supergroup_past_the_bound_is_asked_and_decided() {
    let chat_id = -1_000_000_000_555_i64;
    let chat = r#"{"@type":"updateNewChat","chat":{"@type":"chat","id":-1000000000555,"type":{"@type":"chatTypeSupergroup","supergroup_id":555,"is_channel":false},"title":"Group 8","positions":[{"@type":"chatPosition","list":{"@type":"chatListMain"},"order":"7301444000000000008","is_pinned":false}]}}"#;
    let mut chats = Chats::new(None);
    feed(&mut chats, chat);
    assert!(chats.knows(chat_id));
    let out = chats.link(ME);
    assert_eq!(out.fetch, [Fetch::Supergroup(555)], "asked at the link");
    assert!(chats.deciding(chat_id) && chats.conv(chat_id).is_none());
    assert_eq!(
        Fetch::Supergroup(555).request(),
        serde_json::json!({"@type": "getSupergroup", "supergroup_id": 555})
    );
    // Once per run: a fact about the chat asks nothing more.
    let again = feed(
        &mut chats,
        r#"{"@type":"updateChatTitle","chat_id":-1000000000555,"title":"Group 9"}"#,
    );
    assert!(again.fetch.is_empty(), "asked twice");
    let sg: tdapi::Supergroup =
        serde_json::from_str(r#"{"id":555,"is_channel":false}"#).expect("a supergroup");
    let mut decided = chats.on_supergroup(sg);
    decided.extend(chats.flush());
    assert!(!chats.deciding(chat_id), "still undecided");
    assert!(chats.conv(chat_id).is_some(), "not mapped");
    assert_eq!(upserts(&decided).len(), 1, "the chat's patch");
}

/// TF13 (review pF4): a user or a supergroup with a restriction reason is HELD like H5 until T7
/// (`Held::Restricted`), a plain one is not. (Mutant: not held.)
#[test]
fn a_restricted_user_or_supergroup_is_held() {
    let mut chats = Chats::new(Some(ME));
    chats.link(ME);
    feed(
        &mut chats,
        r#"{"@type":"updateUser","user":{"@type":"user","id":601,"first_name":"Fay","last_name":"Sample","phone_number":"","is_contact":false,"type":{"@type":"userTypeRegular"},"restriction_info":{"@type":"restrictionInfo","restriction_reason":"synthetic-restriction","has_sensitive_content":false}}}"#,
    );
    feed(
        &mut chats,
        &chat_line(601, "chatListMain", 5, false, (true, 0), (0, 0)),
    );
    feed(&mut chats, &user_line(602, "", false));
    feed(
        &mut chats,
        &chat_line(602, "chatListMain", 6, false, (true, 0), (0, 0)),
    );
    feed(
        &mut chats,
        r#"{"@type":"updateSupergroup","supergroup":{"@type":"supergroup","id":603,"is_channel":false,"restriction_info":{"@type":"restrictionInfo","restriction_reason":"synthetic-restriction","has_sensitive_content":false}}}"#,
    );
    feed(
        &mut chats,
        r#"{"@type":"updateNewChat","chat":{"@type":"chat","id":-1000000000603,"type":{"@type":"chatTypeSupergroup","supergroup_id":603,"is_channel":false},"title":"Group 3","positions":[{"@type":"chatPosition","list":{"@type":"chatListMain"},"order":"7","is_pinned":false}]}}"#,
    );
    assert_eq!(chats.held(601), Some(Held::Restricted), "a restricted user");
    assert_eq!(chats.held(602), None, "a plain user");
    assert!(chats.conv(602).is_some() && chats.conv(601).is_none());
    assert_eq!(
        chats.held(-1_000_000_000_603),
        Some(Held::Restricted),
        "a restricted supergroup"
    );
}

/// Q-L9: past the bound the machine learns nothing more before the link; a chat it dropped is
/// asked with `getChat` at the link, and its answer is learnt.
#[test]
fn a_chat_dropped_by_the_bound_is_asked_at_the_link() {
    let mut chats = Chats::new(None);
    for id in 1..=LEARN_BOUND {
        feed(
            &mut chats,
            &format!(
                r#"{{"@type":"updateUser","user":{{"@type":"user","id":{},"type":{{"@type":"userTypeRegular"}}}}}}"#,
                10_000 + id
            ),
        );
    }
    let chat = r#"{"@type":"updateNewChat","chat":{"@type":"chat","id":-301,"type":{"@type":"chatTypeBasicGroup","basic_group_id":301},"title":"Group 7","positions":[{"@type":"chatPosition","list":{"@type":"chatListMain"},"order":"7301444000000000007","is_pinned":false}]}}"#;
    assert!(feed(&mut chats, chat).is_empty());
    assert_eq!(chats.dropped(), 1);
    assert!(!chats.knows(-301));
    let out = chats.link(ME);
    assert_eq!(out.fetch, [Fetch::Chat(-301)]);
    let Some(Update::Receive(tdapi::Receive::NewChat(answer))) = tdapi::parse_update(chat) else {
        panic!("the synthetic line parses");
    };
    let mut learnt = chats.on_chat(*answer);
    learnt.extend(chats.flush());
    assert_eq!(upserts(&learnt).len(), 1);
}

/// TG-4 K6b: the bound a test lowers (`Timings::learn_bound`, through the session) is the one the
/// machine applies. With two objects learnt, a supergroup and then its chat come past the bound:
/// neither is learnt, the chat is asked with `getChat` at the link (the first-link path of a big
/// account, reachable with a handful of objects), and the unlowered machine learns both. (Mutant:
/// `bound()` ignores the lowered value -> the chat is known and nothing is asked.)
#[test]
fn a_lowered_bound_drops_what_comes_past_it_and_asks_the_chat_at_the_link() {
    let user = |id: i64| {
        format!(
            r#"{{"@type":"updateUser","user":{{"@type":"user","id":{id},"type":{{"@type":"userTypeRegular"}}}}}}"#
        )
    };
    let supergroup = r#"{"@type":"updateSupergroup","supergroup":{"@type":"supergroup","id":777,"is_channel":false}}"#;
    let chat = r#"{"@type":"updateNewChat","chat":{"@type":"chat","id":-1000000000777,"type":{"@type":"chatTypeSupergroup","supergroup_id":777,"is_channel":false},"title":"Group 9","positions":[{"@type":"chatPosition","list":{"@type":"chatListMain"},"order":"7","is_pinned":false}]}}"#;
    let told = |chats: &mut Chats| {
        feed(chats, &user(10_001));
        feed(chats, &user(10_002));
        feed(chats, supergroup);
        feed(chats, chat);
    };
    let mut lowered = Chats::new(None);
    lowered.set_learn_bound(2);
    told(&mut lowered);
    assert!(!lowered.knows(-1_000_000_000_777), "told past the bound");
    assert_eq!(lowered.dropped(), 2, "the supergroup and its chat");
    assert_eq!(
        lowered.link(ME).fetch,
        [Fetch::Chat(-1_000_000_000_777)],
        "asked at the link"
    );
    let mut plain = Chats::new(None);
    told(&mut plain);
    assert!(
        plain.knows(-1_000_000_000_777),
        "the product's bound is 4,096"
    );
    assert_eq!(plain.dropped(), 0);
    assert!(plain.link(ME).fetch.is_empty());
}

/// Critic L6: a chat's patch before its first message row of a run is restated whatever its
/// hash; later rows' patches are gated.
#[test]
fn the_first_row_of_a_run_restates_its_chat() {
    let mut chats = Chats::new(Some(ME));
    chats.link(ME);
    feed(
        &mut chats,
        r#"{"@type":"updateNewChat","chat":{"@type":"chat","id":-301,"type":{"@type":"chatTypeBasicGroup","basic_group_id":301},"title":"Group 7","positions":[]}}"#,
    );
    let first = chats.row_upsert(-301).expect("a mapped chat");
    let second = chats.row_upsert(-301).expect("a mapped chat");
    assert_eq!(
        (first.gate, second.gate),
        (Some(Gate::Restate(-301)), Some(Gate::Chat(-301)))
    );
    assert!(chats.row_upsert(4242).is_none(), "an unknown chat");
}

// --- C5b: the patches, the pins, the users -------------------------------------------------

/// The clock of the C5b tests (the synthetic epoch's milliseconds).
const NOW: i64 = 1_700_000_000_000;

/// The last conversation patch per key in `out`.
fn patches(out: &Out) -> std::collections::BTreeMap<ConvKey, kourier_model::ConversationPatch> {
    out.staged
        .rows
        .iter()
        .flat_map(|r| &r.events)
        .filter_map(|e| match &e.event {
            BackendEvent::ConversationUpsert(p) => Some((p.key.clone(), p.clone())),
            _ => None,
        })
        .collect()
}

/// The contact patches of `out`, with their gates.
fn contacts(out: &Out) -> Vec<(kourier_model::ContactPatch, Option<Gate>)> {
    out.staged
        .rows
        .iter()
        .flat_map(|r| &r.events)
        .filter_map(|e| match &e.event {
            BackendEvent::ContactUpsert(p) => Some((p.clone(), e.gate)),
            _ => None,
        })
        .collect()
}

/// The pinned lists of `out`, in order.
fn pin_lists(out: &Out) -> Vec<Vec<ConvKey>> {
    out.staged
        .rows
        .iter()
        .flat_map(|r| &r.events)
        .filter_map(|e| match &e.event {
            BackendEvent::PinnedConversations(keys) => Some(keys.clone()),
            _ => None,
        })
        .collect()
}

/// A synthetic regular user `id` (`phone`: digits or empty).
fn user_line(id: i64, phone: &str, contact: bool) -> String {
    format!(
        r#"{{"@type":"updateUser","user":{{"@type":"user","id":{id},"first_name":"Fay","last_name":"Sample","phone_number":"{phone}","is_contact":{contact},"type":{{"@type":"userTypeRegular"}}}}}}"#
    )
}

/// A synthetic private chat `id` in `list` (`chatListMain`/`chatListArchive`) with its order and
/// pin, its mute (`use_default_mute_for`, `mute_for`) and its unread state (count, read up to).
fn chat_line(
    id: i64,
    list: &str,
    order: i64,
    pinned: bool,
    mute: (bool, i64),
    unread: (i64, i64),
) -> String {
    format!(
        r#"{{"@type":"updateNewChat","chat":{{"@type":"chat","id":{id},"type":{{"@type":"chatTypePrivate","user_id":{id}}},"title":"Fay Sample","positions":[{{"@type":"chatPosition","list":{{"@type":"{list}"}},"order":"{order}","is_pinned":{pinned}}}],"notification_settings":{{"@type":"chatNotificationSettings","use_default_mute_for":{},"mute_for":{}}},"unread_count":{},"last_read_inbox_message_id":{}}}}}"#,
        mute.0, mute.1, unread.0, unread.1
    )
}

/// A linked machine at [`NOW`] fed `lines`, and what that gave with a flush at the end.
fn linked(lines: &[String]) -> (Chats, Out) {
    let mut chats = Chats::new(Some(ME));
    chats.set_now(NOW);
    let mut out = chats.link(ME);
    out.extend(feed_all(&mut chats, lines));
    (chats, out)
}

/// His archive mapping (brief §3.2): `rec-fresh-start.jsonl` puts chat 203 in the archive list;
/// it is the one archived conversation, every other listed chat is `archived: false`.
#[test]
fn archive_is_chat_list_archive() {
    let rec = Recording::load("rec-fresh-start.jsonl");
    let (lines, answers) = objects(&rec.parts);
    assert_eq!(answers, 16, "recorded answers used");
    let (_, out) = linked(&lines);
    let all = patches(&out);
    assert_eq!(all.get(&direct(203)).map(|p| p.archived), Some(Some(true)));
    let others: Vec<_> = all.iter().filter(|(k, _)| **k != direct(203)).collect();
    assert!(others.len() >= 4, "the main list's conversations");
    for (key, p) in others {
        assert_eq!(p.archived, Some(false), "{key:?}");
    }
}

/// Critic L6 and the gate: `rec-restart-saved-pts.jsonl`'s second helper run tells every chat
/// again (`updateNewChat`, a read state of 0, then the real one); committed into one backend.db,
/// a chat whose patch is the same after both runs is written once, in the first.
#[test]
fn a_restart_restates_nothing_unchanged() {
    let rec = Recording::load("rec-restart-saved-pts.jsonl");
    let (run1, a1) = objects(rec.parts.iter().take(1));
    let (run2, a2) = objects(rec.parts.iter().skip(1));
    assert_eq!((a1, a2), (16, 20), "recorded answers used");
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = crate::db::open(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from_bytes([7; 32]),
    )
    .expect("open");
    let run = |lines: &[String]| {
        let (_, out) = linked(lines);
        let done = crate::store::commit_rows(&conn, &out.staged, NOW).expect("commit");
        (patches(&out), done)
    };
    let (first, _) = run(&run1);
    let (second, done) = run(&run2);
    let written: Vec<ConvKey> = kourier_db::outbox::peek(&conn, 10_000)
        .expect("peek")
        .into_iter()
        .filter(|r| done.seqs.contains(&r.seq))
        .flat_map(|r| kourier_backend_api::decode_outbox_row(&r.envelope).expect("decode"))
        .filter_map(|e| match e {
            BackendEvent::ConversationUpsert(p) => Some(p.key),
            _ => None,
        })
        .collect();
    let same: Vec<&ConvKey> = second
        .iter()
        .filter(|(k, p)| first.get(*k) == Some(*p))
        .map(|(k, _)| k)
        .collect();
    // The other chats got messages while the helper was off (their unread counts changed).
    assert!(same.len() >= 2, "unchanged chats: {}", same.len());
    for key in same {
        assert!(
            !written.contains(key),
            "an unchanged chat was stated again: {key:?}"
        );
    }
    assert!(done.gated >= 2, "gated: {}", done.gated);
}

/// Brief §3.2: a chat that takes its scope's default is muted by the scope's `mute_for` and
/// follows a change of it; a chat's own `mute_for` wins otherwise; a mute past a year is for ever.
#[test]
fn mute_uses_the_scope_default() {
    let scope = |secs: i64| {
        format!(
            r#"{{"@type":"updateScopeNotificationSettings","scope":{{"@type":"notificationSettingsScopePrivateChats"}},"notification_settings":{{"@type":"scopeNotificationSettings","mute_for":{secs}}}}}"#
        )
    };
    let (mut chats, out) = linked(&[
        scope(3600),
        user_line(207, "", false),
        user_line(208, "", false),
        user_line(209, "", false),
        chat_line(207, "chatListMain", 7, false, (true, 0), (0, 0)),
        chat_line(208, "chatListMain", 8, false, (false, 0), (0, 0)),
        chat_line(
            209,
            "chatListMain",
            9,
            false,
            (false, 400 * 24 * 3600),
            (0, 0),
        ),
    ]);
    let p = patches(&out);
    assert_eq!(
        p[&direct(207)].muted_until,
        Patch::Set((NOW / 1000 + 3600) * 1000)
    );
    assert_eq!(p[&direct(208)].muted_until, Patch::Clear);
    assert_eq!(p[&direct(209)].muted_until, Patch::Set(i64::MAX));
    let mut unmuted = feed(&mut chats, &scope(0));
    unmuted.extend(chats.flush());
    let p = patches(&unmuted);
    assert_eq!(
        p[&direct(207)].muted_until,
        Patch::Clear,
        "the scope's change"
    );
    assert!(
        !p.contains_key(&direct(208)),
        "a chat with its own mute is not restated"
    );
}

/// Brief §3.2: `PinnedConversations` lists the main list's pins by order, highest first; an
/// archive pin is no Kourier pin; an unpin states the shorter list.
#[test]
fn pins_follow_order_desc() {
    let mut lines = Vec::new();
    for (id, list, order, pinned) in [
        (205, "chatListMain", 5, true),
        (209, "chatListMain", 9, true),
        (207, "chatListMain", 7, true),
        (208, "chatListArchive", 8, true),
        (206, "chatListMain", 6, false),
    ] {
        lines.push(user_line(id, "", false));
        lines.push(chat_line(id, list, order, pinned, (true, 0), (0, 0)));
    }
    let (mut chats, out) = linked(&lines);
    assert_eq!(
        pin_lists(&out).last(),
        Some(&vec![direct(209), direct(207), direct(205)])
    );
    let mut unpinned = feed(
        &mut chats,
        r#"{"@type":"updateChatPosition","chat_id":207,"position":{"@type":"chatPosition","list":{"@type":"chatListMain"},"order":"7","is_pinned":false}}"#,
    );
    unpinned.extend(chats.flush());
    assert_eq!(pin_lists(&unpinned), [vec![direct(209), direct(205)]]);
    assert!(
        pin_lists(&chats.flush()).is_empty(),
        "an unchanged list is not stated"
    );
}

/// Critic M1: Telegram's conversations take edits without a window (the server validates them);
/// received deletes have none until KT-2; read receipts and typing are on (his TG6).
#[test]
fn edits_are_taken_without_a_window() {
    let (_, out) = linked(&[
        user_line(207, "", false),
        chat_line(207, "chatListMain", 7, false, (true, 0), (0, 0)),
    ]);
    let features = patches(&out)[&direct(207)]
        .features
        .clone()
        .expect("features");
    assert_eq!(
        features.edit_receive,
        Some(kourier_model::Window {
            max_age_secs: None,
            max_count: None
        })
    );
    assert_eq!(features.delete_receive, None);
    assert!(features.read_receipts && features.typing);
}

/// Critic L3, brief §3.5: a needed user's contact patch carries the number with the verdict
/// `number_shown`, a user who hides it `number_shown: Some(false)` and no number; a user Kourier
/// has no reason to know states nothing, a contact does; the own account never.
#[test]
fn phone_identifiers_carry_the_number_verdict() {
    let (mut chats, out) = linked(&[
        user_line(207, "99900000207", false),
        chat_line(207, "chatListMain", 7, false, (true, 0), (0, 0)),
        user_line(208, "", false),
        chat_line(208, "chatListMain", 8, false, (true, 0), (0, 0)),
        user_line(209, "99900000209", false),
        user_line(210, "", true),
        user_line(ME, "99900000100", true),
    ]);
    let stated = contacts(&out);
    let of = |id: i64| {
        stated
            .iter()
            .find(|(p, _)| p.user.as_str() == format!("tg:{id}"))
            .map(|(p, g)| (p.clone(), *g))
    };
    let (shown, gate) = of(207).expect("the peer of a listed chat");
    assert_eq!(gate, Some(Gate::User(207)));
    assert_eq!(shown.number_shown, Some(true));
    let ids = shown.identifiers.expect("identifiers");
    assert!(
        ids.iter()
            .any(|i| i.kind == kourier_model::IdentifierKind::Phone && i.value == "+99900000207")
    );
    let (hidden, _) = of(208).expect("the peer of a listed chat");
    assert_eq!(hidden.number_shown, Some(false));
    assert!(hidden.identifiers.expect("identifiers").is_empty());
    assert!(of(209).is_none(), "a user Kourier has no reason to know");
    assert!(of(210).is_some(), "a contact");
    assert!(of(ME).is_none(), "the own account");
    assert!(chats.author_upsert(ME).is_none());
    assert!(chats.author_upsert(209).is_some(), "a message's author");
    let printed = format!("{out:?}");
    assert!(!printed.contains("99900000207") && !printed.contains("Fay"));
}

/// Critic M4: the hint is the unread messages WITHOUT rows: a delivered incoming message above
/// the read position is never in it (nor one seeded from `delivered` at a start); an outgoing one
/// never counts; a read moves rows out of the count.
#[test]
fn unread_hint_never_counts_a_delivered_unread_row() {
    let step = 1_i64 << 20;
    let (mut chats, _) = linked(&[
        user_line(207, "", false),
        chat_line(207, "chatListMain", 7, false, (true, 0), (4, step)),
    ]);
    chats.on_delivered(207, 2 * step, false);
    chats.on_delivered(207, 3 * step, false);
    chats.on_delivered(207, 4 * step, true);
    let hint = |e: Option<crate::store::Event>| match e.map(|e| e.event) {
        Some(BackendEvent::ConversationUpsert(p)) => p.unread_hint,
        _ => None,
    };
    assert_eq!(
        hint(chats.row_upsert(207)),
        Some(2),
        "4 unread, 2 of them rows"
    );
    chats.seed_unread(207, [5 * step]);
    assert_eq!(hint(chats.row_upsert(207)), Some(1), "a seeded row");
    let mut read = feed(
        &mut chats,
        &format!(
            r#"{{"@type":"updateChatReadInbox","chat_id":207,"last_read_inbox_message_id":{},"unread_count":2}}"#,
            3 * step
        ),
    );
    read.extend(chats.flush());
    assert_eq!(
        patches(&read)[&direct(207)].unread_hint,
        Some(1),
        "2 unread: the seeded row and one without a row"
    );
}

/// L-8, critic L2: a chat as an author (`messageSenderChat`) is the contact `tg:<signed chat id>`
/// (a leading `-` passes the scoped-id rule) with the chat's title, gated by that id.
#[test]
fn a_chat_author_is_its_signed_id() {
    let mut chats = Chats::new(Some(ME));
    chats.link(ME);
    feed(
        &mut chats,
        r#"{"@type":"updateNewChat","chat":{"@type":"chat","id":-1000000000403,"type":{"@type":"chatTypeSupergroup","supergroup_id":403,"is_channel":true},"title":"Channel 3","positions":[]}}"#,
    );
    let event = chats
        .sender_chat_upsert(-1_000_000_000_403)
        .expect("a known chat");
    assert_eq!(event.gate, Some(Gate::User(-1_000_000_000_403)));
    let BackendEvent::ContactUpsert(p) = &event.event else {
        panic!("a contact patch");
    };
    assert_eq!(p.user.as_str(), "tg:-1000000000403");
    assert_eq!(p.profile_name, Patch::Set("Channel 3".to_owned()));
    assert!(event.event.validate_ids().is_ok());
    assert!(chats.sender_chat_upsert(-5).is_none(), "an unknown chat");
}

// --- C6a: the message machine --------------------------------------------------------------

mod messages;
