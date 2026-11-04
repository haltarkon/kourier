//! Quiet notices stay out of the conversation list (2026-10-15, `src/notices.rs`; the user:
//! "'Your safety number with ...' messages should have toggle to disable showing in All
//! chat as latest message in chat. It looks like digital garbage."):
//!
//! - a safety number that changed (in a 1:1 chat and in a group), a refreshed session, a
//!   changed number and an accepted request are QUIET: by default they are neither a
//!   conversation's preview nor its time nor its place in the list, and they stay in the
//!   chat itself; group changes, timer changes and calls still show;
//! - the list never leaves a chat out for what it holds: a LISTED chat (an empty group, a chat
//!   he cleared, a chat whose messages all expired) stays listed when a quiet notice lands in
//!   it; the chats that were all quiet when core.db became schema 15 are hidden once by the
//!   migration, in the state of a deleted chat, and a real message lists them again;
//! - the generated column `messages.quiet` and `SystemEvent::is_quiet` are one rule.

use std::time::Duration;

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_core::facade::{
    ConversationListQuery, ConversationRow, MessageRow, Query, QueryData, TimelineAnchor,
    TimelineQuery, UiCommand, UiEvent,
};
use kourier_core::testing::IngestHarness;
use kourier_fake::Flavor;
use kourier_fake::scripts::{T0, remote};
use kourier_model::{
    Body, CallDirection, CallLine, CallMedia, CallState, ConvKey, ConversationPatch, MsgIdScope,
    NetworkKind, Origin, Patch, Placeholder, Poll, SystemEvent, UserId,
};

use crate::support::send::script;
use crate::support::{Env, add_linked};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
const BOB: &str = "aci:00000000-0000-4000-8000-000000000b0b";
const CAROL: &str = "aci:00000000-0000-4000-8000-00000000ca01";

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn alice() -> UserId {
    uid(ALICE)
}

fn d_alice() -> ConvKey {
    ConvKey::Direct(alice())
}

fn d_bob() -> ConvKey {
    ConvKey::Direct(uid(BOB))
}

fn group() -> ConvKey {
    ConvKey::Group("gv2:cXVpZXQtbm90aWNlcw".into())
}

fn harness() -> (tempfile::TempDir, IngestHarness) {
    let dir = tempfile::tempdir().expect("tempdir");
    let h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([6; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness");
    (dir, h)
}

/// A notice of `conv` made by `event` at `at` (received 500 ms later, like `remote`'s texts),
/// as the Signal backend reports one: from the contact, silent, its id the time.
fn notice(conv: &ConvKey, at: i64, event: SystemEvent) -> IncomingMessage {
    let mut m = IncomingMessage::new(
        conv.clone(),
        format!("notice-{at}"),
        Origin::Remote {
            author: alice(),
            device: None,
        },
        Body::System(event),
        at,
        // (the list orders by the time a row was RECEIVED, as `remote` makes it)
        at + 500,
    );
    m.silent = true;
    m
}

/// A text of `conv` from Alice (or from Bob in his own chat) at `at`.
fn text(conv: &ConvKey, at: i64) -> IncomingMessage {
    let by = match conv {
        ConvKey::Direct(u) => u.clone(),
        _ => alice(),
    };
    remote(conv, &by, &format!("text-{at}"), at, "hello")
}

/// The scene of the list tests: `a` has a text at 100, Bob's chat has one at 200, so the list
/// reads Bob, `a`.
fn scene(a: &ConvKey) -> (tempfile::TempDir, IngestHarness) {
    let (dir, mut h) = harness();
    h.apply(vec![
        BackendEvent::Message(text(a, 100)),
        BackendEvent::Message(text(&d_bob(), 200)),
    ])
    .expect("the two chats");
    (dir, h)
}

fn list(h: &IngestHarness) -> Vec<ConversationRow> {
    h.conversations(&ConversationListQuery::default())
        .expect("list")
        .rows
}

/// Who a row is: `alice`, `bob`, `carol` or `group`.
fn who(r: &ConversationRow) -> &'static str {
    match r.peer.as_ref().map(UserId::as_str) {
        Some(ALICE) => "alice",
        Some(BOB) => "bob",
        Some(CAROL) => "carol",
        _ => "group",
    }
}

fn order(h: &IngestHarness) -> Vec<&'static str> {
    list(h).iter().map(who).collect()
}

fn row_of(h: &IngestHarness, name: &str) -> ConversationRow {
    list(h)
        .into_iter()
        .find(|r| who(r) == name)
        .unwrap_or_else(|| panic!("{name} is listed"))
}

fn name_of(key: &ConvKey) -> &'static str {
    match key {
        ConvKey::Direct(u) if u.as_str() == ALICE => "alice",
        ConvKey::Direct(u) if u.as_str() == BOB => "bob",
        ConvKey::Direct(_) => "carol",
        _ => "group",
    }
}

fn timeline(h: &IngestHarness, name: &str) -> Vec<MessageRow> {
    h.timeline(&TimelineQuery {
        conv: row_of(h, name).id,
        anchor: TimelineAnchor::Newest,
        limit: 100,
    })
    .expect("timeline")
    .rows
}

/// Rule 1: a quiet notice that arrives after the newest text of another chat does not lift
/// its chat: the list keeps its order, the preview and the sort time stay those of the last
/// real message, and the notice is a row of the chat.
#[test]
fn a_quiet_notice_neither_previews_nor_moves_its_chat() {
    let cases: Vec<(&str, ConvKey, SystemEvent)> = vec![
        (
            "safety number, 1:1",
            d_alice(),
            SystemEvent::IdentityKeyChanged { user: alice() },
        ),
        (
            "safety number, in a group",
            group(),
            SystemEvent::IdentityKeyChanged { user: alice() },
        ),
        (
            "session refreshed",
            d_alice(),
            SystemEvent::SessionRefreshed { user: alice() },
        ),
        (
            "number changed",
            d_alice(),
            SystemEvent::NumberChanged { user: alice() },
        ),
        (
            "request accepted",
            d_alice(),
            SystemEvent::MessageRequestAccepted,
        ),
    ];
    for (label, key, event) in cases {
        let (_dir, mut h) = scene(&key);
        let name = name_of(&key);
        assert_eq!(order(&h), ["bob", name], "{label}: before");
        let before = row_of(&h, name);
        h.apply(vec![BackendEvent::Message(notice(&key, 300, event))])
            .expect(label);
        assert_eq!(
            order(&h),
            ["bob", name],
            "{label}: the notice lifted its chat"
        );
        let after = row_of(&h, name);
        assert_eq!(
            after.preview.as_ref().map(|p| p.sent_at),
            Some(100),
            "{label}: the preview is the last real message"
        );
        assert!(
            after.preview.as_ref().is_some_and(|p| p.system.is_none()),
            "{label}: no notice in the preview"
        );
        assert_eq!(
            after.last_activity, before.last_activity,
            "{label}: the sort time"
        );
        assert!(
            timeline(&h, name)
                .iter()
                .any(|r| r.sent_at == 300 && matches!(r.body, Body::System(_))),
            "{label}: the notice stays in the chat"
        );
    }
}

/// Rule 2 (what the user kept): group changes, a member who left, timer changes and calls
/// still lift their chat and are its preview.
#[test]
fn loud_notices_still_lift_their_chat() {
    let by = Some(alice());
    let cases: Vec<(&str, ConvKey, SystemEvent)> = vec![
        (
            "timer changed",
            d_alice(),
            SystemEvent::TimerChanged {
                by: by.clone(),
                timer: Some(Duration::from_secs(3600)),
            },
        ),
        (
            "members added",
            group(),
            SystemEvent::MembersAdded {
                by: by.clone(),
                users: vec![uid(BOB)],
            },
        ),
        (
            "members removed",
            group(),
            SystemEvent::MembersRemoved {
                by: by.clone(),
                users: vec![uid(BOB)],
            },
        ),
        (
            "a member left",
            group(),
            SystemEvent::MemberLeft { user: uid(BOB) },
        ),
        (
            "title changed",
            group(),
            SystemEvent::TitleChanged {
                by: by.clone(),
                title: "x".into(),
            },
        ),
        (
            "avatar changed",
            group(),
            SystemEvent::AvatarChanged { by: by.clone() },
        ),
        (
            "description changed",
            group(),
            SystemEvent::DescriptionChanged { by: by.clone() },
        ),
        (
            "group created",
            group(),
            SystemEvent::GroupCreated { by: by.clone() },
        ),
        ("joined", group(), SystemEvent::Joined),
    ];
    for (label, key, event) in cases {
        let (_dir, mut h) = scene(&key);
        let name = name_of(&key);
        h.apply(vec![BackendEvent::Message(notice(
            &key,
            300,
            event.clone(),
        ))])
        .expect(label);
        assert_eq!(
            order(&h),
            [name, "bob"],
            "{label}: the notice lifts its chat"
        );
        let preview = row_of(&h, name).preview.expect("a preview");
        assert_eq!(preview.sent_at, 300, "{label}");
        assert_eq!(
            preview.system,
            Some(event),
            "{label}: the notice is the preview"
        );
    }
    // A missed call is a line of the chat like any other loud row.
    let key = d_alice();
    let (_dir, mut h) = scene(&key);
    let line = CallLine::signal(CallDirection::Incoming, false, CallState::Missed, 300)
        .with_media(CallMedia::Voice, 300);
    h.apply(vec![BackendEvent::Message(IncomingMessage::call(
        key.clone(),
        "77",
        Origin::Remote {
            author: alice(),
            device: None,
        },
        line,
        300,
        800,
    ))])
    .expect("a missed call");
    assert_eq!(order(&h), ["alice", "bob"], "a missed call lifts its chat");
    assert!(
        row_of(&h, "alice")
            .preview
            .is_some_and(|p| matches!(p.system, Some(SystemEvent::Call(_))))
    );
}

/// Rule 4 as the lead decided it (F1, 2026-10-15): the list never leaves a chat out for what
/// it holds. A chat that is LISTED stays listed when a quiet notice lands in it, with the
/// switch off or on: an empty group (what a link leaves, a member's key then changes), a
/// chat he cleared (the contract says it "stays in the list, empty") and a chat whose
/// messages all expired. It has no preview with the switch off (and sorts after the chats
/// that have one) and previews the notice with it on.
#[test]
fn a_listed_chat_stays_listed_when_a_quiet_notice_lands_in_it() {
    let quiet = |key: &ConvKey, at| {
        BackendEvent::Message(notice(
            key,
            at,
            SystemEvent::IdentityKeyChanged { user: alice() },
        ))
    };
    // Bob has a text at 200 (the chat that must keep its place in front of the empty ones).
    let bob_text = || BackendEvent::Message(text(&d_bob(), 200));

    // 1. An empty group, upserted and listed, that gets a key change of one of its members.
    let (_dir, mut h) = harness();
    h.apply(vec![
        bob_text(),
        BackendEvent::ConversationUpsert(ConversationPatch::new(group())),
    ])
    .expect("a chat and an empty group");
    assert_eq!(order(&h), ["bob", "group"], "the empty group is listed");
    h.apply(vec![quiet(&group(), 300)]).expect("a key change");
    assert_eq!(order(&h), ["bob", "group"], "and stays listed");
    assert!(
        row_of(&h, "group").preview.is_none(),
        "with no preview while the notices are hidden"
    );
    assert_eq!(timeline(&h, "group").len(), 1, "the notice is in the chat");
    h.set_quiet_notices(true).expect("show");
    assert_eq!(
        order(&h),
        ["group", "bob"],
        "shown: it previews the notice and lifts the chat"
    );
    h.set_quiet_notices(false).expect("hide");
    assert_eq!(order(&h), ["bob", "group"], "hidden again: as before");

    // 2. A chat he cleared stays in the list, empty; the next notice leaves it there.
    let (_dir, mut h) = scene(&d_alice());
    let alice_id = row_of(&h, "alice").id;
    h.clear_conversation(alice_id, false).expect("clear");
    assert_eq!(order(&h), ["bob", "alice"], "a cleared chat is listed");
    assert!(timeline(&h, "alice").is_empty(), "and empty");
    h.apply(vec![quiet(&d_alice(), 300)]).expect("a key change");
    assert_eq!(order(&h), ["bob", "alice"], "a notice leaves it listed");
    assert_eq!(timeline(&h, "alice").len(), 1);

    // 3. A chat whose messages all expired, then a notice.
    let (_dir, mut h) = harness();
    let mut mortal = text(&d_alice(), 100);
    mortal.expiry = Some(kourier_model::Expiry {
        kind: kourier_model::DisappearKind::AfterSend,
        timer: Duration::from_secs(5),
        started_at: Some(1_000),
    });
    h.set_now(1_000);
    h.apply(vec![BackendEvent::Message(mortal), bob_text()])
        .expect("a message with a timer");
    assert_eq!(order(&h), ["bob", "alice"]);
    h.set_now(7_000);
    h.sweep().expect("the timer ran out");
    assert!(
        timeline(&h, "alice").is_empty(),
        "nothing is left of its messages"
    );
    assert_eq!(order(&h), ["bob", "alice"], "the empty chat is listed");
    h.apply(vec![quiet(&d_alice(), 8_000)])
        .expect("a key change");
    assert_eq!(order(&h), ["bob", "alice"], "and stays listed");
}

/// One value of EVERY [`SystemEvent`] variant, so that the column is compared with
/// [`SystemEvent::is_quiet`] for each. The `match` below names every variant without a `_`
/// arm, but a pattern without a value in `all` compiles and is never met by the loop: that
/// every variant has a value is `every_kind_holds_a_value_of_every_variant_serde_knows`, whose
/// list of variants comes from serde itself.
fn every_kind() -> Vec<SystemEvent> {
    let user = alice;
    let all = vec![
        SystemEvent::MembersAdded {
            by: Some(user()),
            users: vec![uid(BOB)],
        },
        SystemEvent::MembersRemoved {
            by: None,
            users: vec![uid(BOB)],
        },
        SystemEvent::MemberLeft { user: uid(BOB) },
        SystemEvent::TitleChanged {
            by: Some(user()),
            title: "t".into(),
        },
        SystemEvent::AvatarChanged { by: None },
        SystemEvent::DescriptionChanged { by: Some(user()) },
        SystemEvent::TimerChanged {
            by: Some(user()),
            timer: None,
        },
        SystemEvent::IdentityKeyChanged { user: user() },
        SystemEvent::SessionRefreshed { user: user() },
        SystemEvent::GroupCreated { by: None },
        SystemEvent::MessageRequestAccepted,
        SystemEvent::Call(
            CallLine::signal(CallDirection::Incoming, false, CallState::Missed, 1)
                .with_media(CallMedia::Voice, 1),
        ),
        SystemEvent::Joined,
        SystemEvent::NumberChanged { user: user() },
        SystemEvent::VerifiedChanged {
            user: user(),
            verified: false,
            other_device: true,
        },
    ];
    for event in &all {
        match event {
            SystemEvent::MembersAdded { .. }
            | SystemEvent::MembersRemoved { .. }
            | SystemEvent::MemberLeft { .. }
            | SystemEvent::TitleChanged { .. }
            | SystemEvent::AvatarChanged { .. }
            | SystemEvent::DescriptionChanged { .. }
            | SystemEvent::TimerChanged { .. }
            | SystemEvent::IdentityKeyChanged { .. }
            | SystemEvent::SessionRefreshed { .. }
            | SystemEvent::GroupCreated { .. }
            | SystemEvent::MessageRequestAccepted
            | SystemEvent::Call(_)
            | SystemEvent::Joined
            | SystemEvent::NumberChanged { .. }
            | SystemEvent::VerifiedChanged { .. } => {}
        }
    }
    all
}

/// The names serde itself holds for the variants of the enum `T` (what its derived
/// `Deserialize` tells `deserialize_enum`; an alias is in the list too).
fn serde_variants<T: serde::de::DeserializeOwned>() -> Vec<&'static str> {
    use serde::de::{Deserializer, Error as _, Visitor};
    struct Probe<'a>(&'a std::cell::RefCell<Vec<&'static str>>);
    impl<'de> Deserializer<'de> for Probe<'_> {
        type Error = serde::de::value::Error;
        fn deserialize_enum<V: Visitor<'de>>(
            self,
            _name: &'static str,
            variants: &'static [&'static str],
            _visitor: V,
        ) -> Result<V::Value, Self::Error> {
            self.0.borrow_mut().extend_from_slice(variants);
            Err(Self::Error::custom("probe"))
        }
        fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Self::Error> {
            Err(Self::Error::custom("not an enum"))
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes
            byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map
            struct identifier ignored_any
        }
    }
    let seen = std::cell::RefCell::new(Vec::new());
    let _ = T::deserialize(Probe(&seen));
    seen.into_inner()
}

/// The serde name of a variant of an externally tagged enum value: the string of a unit
/// variant, the one key of the others.
fn serde_variant_of<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value).expect("json") {
        serde_json::Value::String(name) => name,
        serde_json::Value::Object(map) if map.len() == 1 => {
            map.keys().next().expect("one key").clone()
        }
        other => panic!("not an externally tagged enum value: {other}"),
    }
}

/// `every_kind()` holds a value of EVERY variant serde knows (F2, the review of 2026-10-15):
/// a new `SystemEvent` whose author forgot to add a value would otherwise skip the column's
/// agreement with `is_quiet` and pass. The names come from serde itself.
#[test]
fn every_kind_holds_a_value_of_every_variant_serde_knows() {
    let mut known = serde_variants::<SystemEvent>();
    // `MissedCall` is the read-only alias of `Call` (the notice a call line replaced).
    known.retain(|name| *name != "MissedCall");
    known.sort_unstable();
    known.dedup();
    assert!(known.len() >= 14, "serde told us the variants: {known:?}");
    let mut held: Vec<String> = every_kind().iter().map(serde_variant_of).collect();
    held.sort_unstable();
    held.dedup();
    assert_eq!(
        held, known,
        "a variant of SystemEvent has no value in every_kind()"
    );
}

/// The column `messages.quiet` is `Body::is_quiet` of the stored body, for a row of every
/// kind of notice and of the other bodies, each stored through the real ingest (a call line
/// through its own path). It also pins serde's spelling the SQL rule depends on: a renamed
/// variant changes the column and not the method, and this fails.
#[test]
fn the_column_and_is_quiet_agree_for_every_kind() {
    let (_dir, mut h) = harness();
    let key = group();
    let mut expected: Vec<(String, bool)> = Vec::new();
    for (i, event) in every_kind().into_iter().enumerate() {
        let at = 1_000 + i64::try_from(i).expect("small");
        let quiet = event.is_quiet();
        let m = match &event {
            SystemEvent::Call(line) => IncomingMessage::call(
                key.clone(),
                &format!("c{i}"),
                Origin::Remote {
                    author: alice(),
                    device: None,
                },
                line.clone(),
                at,
                at,
            ),
            _ => notice(&key, at, event),
        };
        expected.push((m.local.to_string(), quiet));
        h.apply(vec![BackendEvent::Message(m)]).expect("a notice");
    }
    // The other bodies: never quiet.
    let others = [
        Body::text("hello"),
        Body::Poll(Poll {
            question: "q".into(),
            options: vec!["a".into(), "b".into()],
            multiple: false,
        }),
        Body::Placeholder(Placeholder::Expired),
        Body::Unsupported {
            kind_hint: "payment".into(),
            summary: None,
        },
    ];
    for (i, body) in others.into_iter().enumerate() {
        let at = 2_000 + i64::try_from(i).expect("small");
        let mut m = text(&key, at);
        m.body = body;
        expected.push((m.local.to_string(), false));
        h.apply(vec![BackendEvent::Message(m)]).expect("a message");
    }
    let mut stored: Vec<(String, i64, String)> = h
        .connection()
        .prepare("SELECT net_local, quiet, body FROM messages")
        .expect("prepare")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    stored.sort();
    assert_eq!(stored.len(), expected.len(), "every row was stored");
    let mut quiet_rows = 0;
    for (local, quiet, body) in &stored {
        let (_, want) = expected
            .iter()
            .find(|(l, _)| l == local)
            .unwrap_or_else(|| panic!("an unexpected row {local}"));
        let parsed: Body = serde_json::from_str(body).expect("the stored body");
        assert_eq!(
            *quiet == 1,
            parsed.is_quiet(),
            "the column and Body::is_quiet differ for {local}: {body}"
        );
        assert_eq!(*quiet == 1, *want, "{local}");
        quiet_rows += *quiet;
    }
    assert_eq!(
        quiet_rows, 5,
        "exactly the five quiet kinds (the verification notice of P5a the fifth)"
    );
}

/// Schema 15 (the migration of this rule): a profile of an earlier build orders its chats
/// by every message, a quiet notice included. The step adds the column and gives every chat
/// the sort time of its newest LISTED row, so the chat a key change had lifted goes back to
/// where its last real message puts it. Rows that are not JSON (nothing writes one, but the
/// column runs on every write of a row, so it must never fail on one) neither fail the step
/// nor any later write.
#[test]
fn schema_15_gives_every_chat_its_sort_time_without_quiet_notices() {
    let (_dir, mut h) = scene(&d_alice());
    h.apply(vec![
        BackendEvent::Message(notice(
            &d_alice(),
            300,
            SystemEvent::IdentityKeyChanged { user: alice() },
        )),
        BackendEvent::Message(notice(
            &d_bob(),
            310,
            SystemEvent::MembersAdded {
                by: None,
                users: vec![alice()],
            },
        )),
    ])
    .expect("a quiet and a loud notice");
    assert_eq!(
        order(&h),
        ["bob", "alice"],
        "schema 15 as built: the key change is quiet"
    );
    let (alice_before, bob_before) = (row_of(&h, "alice"), row_of(&h, "bob"));
    // The state schema 14 left: no column, and the sort time of every message.
    h.connection()
        .execute_batch(
            "ALTER TABLE messages DROP COLUMN quiet;
             UPDATE conversations SET last_activity = (SELECT max(m.sort_key) FROM messages m
               WHERE m.conversation_id = conversations.id
                 AND (m.deleted IS NULL OR m.deleted <> 'ForMe') AND m.expiry_hidden = 0);
             INSERT INTO messages(account_id, conversation_id, net_scope, net_author, net_local,
               origin, sort_key, sent_at, received_at, body_kind, body)
             SELECT account_id, id, 0, 'x', 'garbage-' || id, 'Remote', 5, 5, 5, 'System',
               'this is not json {' FROM conversations;",
        )
        .expect("the old state");
    let sort_time = |h: &IngestHarness, conv: &str| -> i64 {
        h.connection()
            .query_row(
                "SELECT last_activity FROM conversations WHERE conv_key = ?1",
                [conv],
                |r| r.get(0),
            )
            .expect("a sort time")
    };
    assert!(
        sort_time(&h, &d_alice().encode()) > alice_before.last_activity,
        "the old rule gives the key change the chat's time"
    );
    h.connection()
        .execute_batch(include_str!("../migrations/0015_quiet_notices.sql"))
        .expect("schema 15");
    assert_eq!(order(&h), ["bob", "alice"]);
    assert_eq!(
        row_of(&h, "alice").last_activity,
        alice_before.last_activity
    );
    assert_eq!(row_of(&h, "bob").last_activity, bob_before.last_activity);
    // The rows that are not JSON: not quiet, and every kind of write goes through.
    let garbage: i64 = h
        .connection()
        .query_row(
            "SELECT count(*) FROM messages WHERE net_local LIKE 'garbage-%' AND quiet = 0",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(garbage, 2);
    h.connection()
        .execute(
            "UPDATE messages SET read_state = 'Read' WHERE net_local LIKE 'garbage-%'",
            [],
        )
        .expect("an update of such a row");
    h.apply(vec![BackendEvent::Message(text(&d_alice(), 400))])
        .expect("a recount of a chat that holds one");
    assert_eq!(order(&h), ["alice", "bob"]);
}

/// A user per number, for the tests that need many chats.
fn peer(n: u32) -> UserId {
    uid(&format!("aci:00000000-0000-4000-8000-{n:012x}"))
}

fn id_of(h: &IngestHarness, key: &ConvKey) -> i64 {
    h.connection()
        .query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [key.encode()],
            |r| r.get(0),
        )
        .expect("a chat")
}

fn is_hidden(h: &IngestHarness, key: &ConvKey) -> bool {
    h.connection()
        .query_row(
            "SELECT hidden FROM conversations WHERE id = ?1",
            [id_of(h, key)],
            |r| r.get(0),
        )
        .expect("a flag")
}

/// How many rows every table of core.db holds: what a step must not change that is not meant
/// to (rows deleted, rows queued for a network).
fn table_counts(h: &IngestHarness) -> std::collections::BTreeMap<String, i64> {
    let tables: Vec<String> = h
        .connection()
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("tables")
        .collect::<Result<_, _>>()
        .expect("rows");
    tables
        .into_iter()
        .map(|t| {
            let n = h
                .connection()
                .query_row(&format!("SELECT count(*) FROM \"{t}\""), [], |r| r.get(0))
                .expect("count");
            (t, n)
        })
        .collect()
}

/// Puts a database that schema 15 migrated back into the state schema 14 left: no column and
/// the sort time of the newest visible row of every chat, a quiet notice included.
const SCHEMA_14: &str = "ALTER TABLE messages DROP COLUMN quiet;
    UPDATE conversations SET hidden = 0, last_activity = (SELECT max(m.sort_key) FROM messages m
      WHERE m.conversation_id = conversations.id
        AND (m.deleted IS NULL OR m.deleted <> 'ForMe') AND m.expiry_hidden = 0);";

/// F1: the chats that exist at the upgrade and hold NOTHING but quiet notices (the garbage
/// quiet notices made before the ingest guard) are hidden once, in the state of a deleted
/// chat: no row deleted, nothing queued for any network. A real message lists such a chat
/// again, a quiet notice never does. Each exemption keeps its chat listed: a pin, a draft, a
/// message request, unread messages, an unread mark; and so does a chat without a visible
/// row (its only notice deleted for him, or hidden by the clock) and one with a real row.
#[test]
fn schema_15_hides_the_all_quiet_chats_once_and_a_real_message_lists_them_again() {
    let (_dir, mut h) = harness();
    let key = |n: u32| ConvKey::Direct(peer(n));
    let quiet = |key: &ConvKey, at| {
        BackendEvent::Message(notice(
            key,
            at,
            SystemEvent::SessionRefreshed { user: alice() },
        ))
    };
    // 1 plain, 2 pinned, 3 draft, 4 request, 5 unread, 6 marked unread, 7 only a notice
    // deleted for him, 8 only a notice hidden by the clock, 9 a text and a notice; and a group.
    let mut pinned = ConversationPatch::new(key(2));
    pinned.pinned_order = Patch::Set(1);
    let mut events = vec![
        BackendEvent::ConversationUpsert(ConversationPatch::new(key(1))),
        BackendEvent::ConversationUpsert(pinned),
    ];
    events
        .extend((3..=9).map(|n| BackendEvent::ConversationUpsert(ConversationPatch::new(key(n)))));
    events.push(BackendEvent::ConversationUpsert(ConversationPatch::new(
        group(),
    )));
    h.apply(events).expect("the chats");
    let mut notices: Vec<_> = (1..=8)
        .map(|n| quiet(&key(n), 300 + i64::from(n)))
        .collect();
    notices.push(quiet(&group(), 320));
    notices.push(BackendEvent::Message(text(&key(9), 100)));
    notices.push(quiet(&key(9), 330));
    h.apply(notices).expect("the notices");
    let ids: Vec<i64> = (1..=9).map(|n| id_of(&h, &key(n))).collect();
    let by_id = |n: u32| ids[usize::try_from(n - 1).expect("small")];
    for sql in [
        format!(
            "UPDATE conversations SET draft = 'x' WHERE id = {}",
            by_id(3)
        ),
        format!(
            "UPDATE conversations SET request_pending = 1 WHERE id = {}",
            by_id(4)
        ),
        format!(
            "UPDATE conversations SET unread_hint = 2, unread = 2 WHERE id = {}",
            by_id(5)
        ),
        format!(
            "UPDATE conversations SET marked_unread = 1 WHERE id = {}",
            by_id(6)
        ),
        format!(
            "UPDATE messages SET deleted = 'ForMe' WHERE conversation_id = {}",
            by_id(7)
        ),
        format!(
            "UPDATE messages SET expiry_hidden = 1 WHERE conversation_id = {}",
            by_id(8)
        ),
        // The control chat's text was read: nothing but its real row keeps it listed.
        format!(
            "UPDATE conversations SET unread = 0 WHERE id = {}",
            by_id(9)
        ),
    ] {
        h.connection().execute(&sql, []).expect("a state");
    }
    assert_eq!(list(&h).len(), 10, "schema 15 as built lists all ten chats");

    // The state schema 14 left, then the step.
    h.connection().execute_batch(SCHEMA_14).expect("schema 14");
    let before = table_counts(&h);
    h.connection()
        .execute_batch(include_str!("../migrations/0015_quiet_notices.sql"))
        .expect("schema 15");
    assert_eq!(
        table_counts(&h),
        before,
        "no row deleted and none queued for any network (outbox, pending commands, jobs)"
    );
    let hidden: Vec<bool> = (1..=9).map(|n| is_hidden(&h, &key(n))).collect();
    assert_eq!(
        hidden,
        [true, false, false, false, false, false, false, false, false],
        "only the plain chat of notices is hidden (pinned, draft, request, unread, marked, \
         no visible row and a real message are not)"
    );
    assert!(is_hidden(&h, &group()), "and the group of notices");
    let listed: std::collections::BTreeSet<i64> = list(&h).iter().map(|r| r.id.0).collect();
    let want: std::collections::BTreeSet<i64> = (2..=9).map(by_id).collect();
    assert_eq!(listed, want, "the list leaves out exactly the two hidden");
    assert_eq!(
        row_of_id(&h, by_id(9)).preview.map(|p| p.sent_at),
        Some(100),
        "the chat with a text previews it"
    );

    // A quiet notice brings nothing back; a real message lists the chat again, its notice
    // still in it.
    h.apply(vec![quiet(&key(1), 500), quiet(&group(), 510)])
        .expect("more notices");
    assert!(is_hidden(&h, &key(1)) && is_hidden(&h, &group()));
    h.set_quiet_notices(true).expect("show");
    assert!(
        is_hidden(&h, &key(1)),
        "the switch does not undo the one-time clean-up: it is the state of a deleted chat"
    );
    assert!(list(&h).iter().all(|r| r.id.0 != by_id(1)));
    h.apply(vec![BackendEvent::Message(text(&key(1), 600))])
        .expect("a real message");
    assert!(!is_hidden(&h, &key(1)), "listed again");
    let again = row_of_id(&h, by_id(1));
    assert_eq!(again.preview.map(|p| p.sent_at), Some(600));
    let notices_left: i64 = h
        .connection()
        .query_row(
            "SELECT count(*) FROM messages WHERE conversation_id = ?1 AND quiet = 1",
            [by_id(1)],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(notices_left, 2, "the notices were never deleted");
    h.apply(vec![BackendEvent::Message(text(&group(), 700))])
        .expect("a real message in the group");
    assert!(!is_hidden(&h, &group()));
}

fn row_of_id(h: &IngestHarness, id: i64) -> ConversationRow {
    list(h)
        .into_iter()
        .find(|r| r.id.0 == id)
        .unwrap_or_else(|| panic!("chat {id} is listed"))
}

/// F3: the step gives a chat the sort time of its newest VISIBLE real message: a text he
/// deleted for himself and a text hidden by the clock, both above it, are not that row.
#[test]
fn schema_15_sorts_a_chat_by_its_newest_visible_real_message() {
    let (_dir, mut h) = scene(&d_alice());
    h.apply(vec![
        BackendEvent::Message(text(&d_alice(), 300)),
        BackendEvent::Message(text(&d_alice(), 310)),
    ])
    .expect("two newer texts");
    h.connection()
        .execute_batch(
            "UPDATE messages SET deleted = 'ForMe' WHERE net_local = 'text-300';
             UPDATE messages SET expiry_hidden = 1 WHERE net_local = 'text-310';",
        )
        .expect("one deleted for him, one hidden by the clock");
    let real: i64 = h
        .connection()
        .query_row(
            "SELECT sort_key FROM messages WHERE net_local = 'text-100'",
            [],
            |r| r.get(0),
        )
        .expect("the real message");
    // A stale sort time that only a correct step can replace.
    h.connection().execute_batch(SCHEMA_14).expect("schema 14");
    h.connection()
        .execute("UPDATE conversations SET last_activity = 99999", [])
        .expect("a stale time");
    h.connection()
        .execute_batch(include_str!("../migrations/0015_quiet_notices.sql"))
        .expect("schema 15");
    assert_eq!(
        row_of(&h, "alice").last_activity,
        real,
        "the real message, not the two rows above it"
    );
    assert_eq!(order(&h), ["bob", "alice"]);
}

/// F4: the command names the chats whose sort time moved AND that the list shows: not a chat
/// hidden by a delete, and none of an account being removed.
#[test]
fn the_choice_announces_only_the_chats_the_list_shows() {
    let (_dir, mut h) = scene(&d_alice());
    let carol = ConvKey::Direct(uid(CAROL));
    h.apply(vec![BackendEvent::Message(text(&carol, 150))])
        .expect("a third chat");
    h.apply(
        [d_alice(), d_bob(), carol.clone()]
            .iter()
            .enumerate()
            .map(|(i, key)| {
                BackendEvent::Message(notice(
                    key,
                    300 + i64::try_from(i).expect("small"),
                    SystemEvent::IdentityKeyChanged { user: alice() },
                ))
            })
            .collect(),
    )
    .expect("a key change in each");
    // Carol's chat is deleted on another device, with its rows left (a notice came after).
    h.connection()
        .execute(
            "UPDATE conversations SET hidden = 1 WHERE id = ?1",
            [id_of(&h, &carol)],
        )
        .expect("hidden");
    let (alice_id, bob_id) = (id_of(&h, &d_alice()), id_of(&h, &d_bob()));
    let mut shown = h.set_quiet_notices(true).expect("show");
    shown.sort_by_key(|c| c.0);
    let mut want = vec![
        kourier_model::ConversationId(alice_id),
        kourier_model::ConversationId(bob_id),
    ];
    want.sort_by_key(|c| c.0);
    assert_eq!(shown, want, "the hidden chat moved too and is not named");

    // Every chat belongs to an account that is being removed: the list shows none.
    h.connection()
        .execute("UPDATE accounts SET state = 'Removing'", [])
        .expect("removing");
    assert_eq!(
        h.set_quiet_notices(false).expect("hide"),
        Vec::new(),
        "chats of an account being removed are not named"
    );
}

/// The setting on the harness: the command's transaction stores the choice, gives every
/// conversation the sort time its newest listed row says, and names exactly the chats whose
/// sort time moved (F4: and that the list shows). A chat with quiet notices only is listed
/// either way.
#[test]
fn the_setting_shows_quiet_notices_in_the_list_and_hides_them_again() {
    let key = d_alice();
    let (_dir, mut h) = scene(&key);
    h.apply(vec![BackendEvent::Message(notice(
        &key,
        300,
        SystemEvent::IdentityKeyChanged { user: alice() },
    ))])
    .expect("a safety number notice");
    assert!(!h.quiet_notices().expect("read"), "hidden by default");
    assert_eq!(order(&h), ["bob", "alice"]);
    let alice_id = row_of(&h, "alice").id;
    let hidden_time = row_of(&h, "alice").last_activity;

    assert_eq!(
        h.set_quiet_notices(true).expect("show"),
        vec![alice_id],
        "only the chat whose sort time moved is named"
    );
    assert!(h.quiet_notices().expect("read"));
    assert_eq!(
        order(&h),
        ["alice", "bob"],
        "shown: the notice lifts its chat"
    );
    let shown = row_of(&h, "alice");
    assert_eq!(shown.preview.as_ref().map(|p| p.sent_at), Some(300));
    assert!(matches!(
        shown.preview.and_then(|p| p.system),
        Some(SystemEvent::IdentityKeyChanged { .. })
    ));
    assert_ne!(shown.last_activity, hidden_time, "the sort time moved");
    assert_eq!(
        h.set_quiet_notices(true).expect("show again"),
        Vec::new(),
        "nothing moves when the choice stays"
    );

    assert_eq!(h.set_quiet_notices(false).expect("hide"), vec![alice_id]);
    assert!(!h.quiet_notices().expect("read"));
    assert_eq!(order(&h), ["bob", "alice"], "hidden again: the old order");
    let back = row_of(&h, "alice");
    assert_eq!(back.last_activity, hidden_time, "the old sort time");
    assert_eq!(back.preview.as_ref().map(|p| p.sent_at), Some(100));

    // A chat that has nothing but a quiet notice stays listed (F1): previewing the notice
    // while shown, with no preview while hidden.
    let (_dir, mut h) = harness();
    h.apply(vec![
        BackendEvent::ConversationUpsert(ConversationPatch::new(d_bob())),
        BackendEvent::Message(notice(
            &d_bob(),
            300,
            SystemEvent::SessionRefreshed { user: alice() },
        )),
    ])
    .expect("a chat with a notice only");
    assert_eq!(order(&h), ["bob"], "hidden: listed all the same");
    assert!(row_of(&h, "bob").preview.is_none(), "with no preview");
    assert_eq!(h.set_quiet_notices(true).expect("show").len(), 1);
    assert_eq!(order(&h), ["bob"], "shown: listed, the notice its preview");
    assert!(
        row_of(&h, "bob")
            .preview
            .is_some_and(|p| matches!(p.system, Some(SystemEvent::SessionRefreshed { .. })))
    );
    assert_eq!(h.set_quiet_notices(false).expect("hide").len(), 1);
    assert_eq!(order(&h), ["bob"], "hidden again: listed, no preview");
    assert!(row_of(&h, "bob").preview.is_none());
}

fn rank(rows: &[ConversationRow], name: &str) -> usize {
    rows.iter()
        .position(|r| who(r) == name)
        .unwrap_or_else(|| panic!("{name} is listed"))
}

/// The command through the real core (the runtime, on the fake network): it is applied
/// before a read behind it, names the chat that moved in `ConversationsChanged` (and no
/// other), the query answers it, and the choice survives a restart of the core: it is the
/// `settings` row, not a mirror in memory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_command_shows_and_hides_them_through_the_core_and_a_restart_keeps_the_choice() {
    let bob_text = remote(&d_bob(), &uid(BOB), "bob-text", T0 + 1_000, "hi");
    let key_change = notice(
        &d_alice(),
        T0 + 2_000,
        SystemEvent::IdentityKeyChanged { user: alice() },
    );
    let fake_script = script("quiet-notices").deliver(vec![
        BackendEvent::Message(bob_text),
        BackendEvent::Message(key_change),
    ]);
    let mut env = Env::new(Flavor::SignalLike, fake_script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;

    let shown_now = |env: &Env| {
        let h = env.handle.clone();
        async move {
            match h.query(Query::QuietNotices).await.expect("query") {
                QueryData::QuietNotices(b) => b,
                other => panic!("unexpected {other:?}"),
            }
        }
    };
    let rows = env.conversations().await;
    assert!(!shown_now(&env).await, "hidden until the user chooses");
    assert!(
        rank(&rows, "bob") < rank(&rows, "alice"),
        "the notice does not lift alice above bob's newer text"
    );
    let alice_id = rows[rank(&rows, "alice")].id;
    // Alice's unread text (the setup's) is hers; the notice is not unread.
    assert_eq!(rows[rank(&rows, "alice")].unread, 1);

    // Show: the next event that names conversations names alice, the read behind the command
    // has the new order, and the query says so.
    env.events = env.handle.subscribe();
    env.handle
        .send(UiCommand::SetQuietNotices { shown: true })
        .expect("send");
    assert!(shown_now(&env).await, "read right behind the command");
    let rows = env.conversations().await;
    assert!(
        rank(&rows, "alice") < rank(&rows, "bob"),
        "shown: the notice lifts alice"
    );
    assert!(
        rows[rank(&rows, "alice")]
            .preview
            .as_ref()
            .is_some_and(|p| matches!(p.system, Some(SystemEvent::IdentityKeyChanged { .. })))
    );
    let named = env
        .event("ConversationsChanged", |e| match e {
            UiEvent::ConversationsChanged { ids } => Some(ids.clone()),
            _ => None,
        })
        .await;
    assert_eq!(named, vec![alice_id], "exactly the chat that moved");

    // The choice is in core.db: a restart keeps it, and the order with it.
    env.restart().await;
    assert!(shown_now(&env).await, "kept across a restart");
    let rows = env.conversations().await;
    assert!(rank(&rows, "alice") < rank(&rows, "bob"));

    env.events = env.handle.subscribe();
    env.handle
        .send(UiCommand::SetQuietNotices { shown: false })
        .expect("send");
    assert!(!shown_now(&env).await);
    let rows = env.conversations().await;
    assert!(
        rank(&rows, "bob") < rank(&rows, "alice"),
        "hidden again: the old order"
    );
    let named = env
        .event("ConversationsChanged", |e| match e {
            UiEvent::ConversationsChanged { ids } => Some(ids.clone()),
            _ => None,
        })
        .await;
    assert_eq!(named, vec![alice_id]);
    env.restart().await;
    assert!(!shown_now(&env).await, "the hidden choice is kept too");
    env.stop().await;
}

fn quiet_kinds() -> Vec<(&'static str, SystemEvent)> {
    vec![
        (
            "safety number",
            SystemEvent::IdentityKeyChanged { user: alice() },
        ),
        (
            "session refreshed",
            SystemEvent::SessionRefreshed { user: alice() },
        ),
        (
            "number changed",
            SystemEvent::NumberChanged { user: alice() },
        ),
        ("request accepted", SystemEvent::MessageRequestAccepted),
    ]
}

fn history(conv: &ConvKey, messages: Vec<IncomingMessage>) -> BackendEvent {
    BackendEvent::HistoryBatch {
        conv: conv.clone(),
        messages,
        has_more: false,
        progress: None,
    }
}

fn count(h: &IngestHarness, sql: &str) -> i64 {
    h.connection()
        .query_row(sql, [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Rule 3, UNCONDITIONAL (whatever the setting says, as Signal Desktop, whose key change
/// gives its chat no activity time): a quiet notice never makes a chat that does not exist
/// (it is dropped, live and in a history batch, for a 1:1 chat and for a group) and never
/// brings back a chat that was deleted (the row is stored, the chat stays deleted); any
/// other row of the same chat does bring it back. The setting changes only what the list
/// shows of a chat that exists.
#[test]
fn a_quiet_notice_opens_no_chat_and_brings_no_deleted_chat_back() {
    for shown in [false, true] {
        let label = if shown { "shown" } else { "hidden" };
        let (_dir, mut h) = harness();
        h.set_quiet_notices(shown).expect("the setting");

        // An unknown peer and an unknown group: no chat, no row.
        let mut at = 300;
        for key in [d_alice(), group()] {
            for (kind, event) in quiet_kinds() {
                at += 1;
                h.apply(vec![BackendEvent::Message(notice(&key, at, event.clone()))])
                    .unwrap_or_else(|e| panic!("{label} {kind}: {e}"));
                at += 1;
                h.apply(vec![history(&key, vec![notice(&key, at, event)])])
                    .unwrap_or_else(|e| panic!("{label} {kind} (history): {e}"));
            }
        }
        assert_eq!(
            count(&h, "SELECT count(*) FROM conversations"),
            0,
            "{label}: a quiet notice made a chat"
        );
        assert_eq!(
            count(&h, "SELECT count(*) FROM messages"),
            0,
            "{label}: a notice for no chat was stored"
        );

        // A chat that was deleted (on another device): the notice is stored in it, and the
        // chat stays deleted.
        h.apply(vec![BackendEvent::Message(text(&d_alice(), 100))])
            .expect("a text");
        h.connection()
            .execute("UPDATE conversations SET hidden = 1", [])
            .expect("deleted");
        assert!(
            order(&h).is_empty(),
            "{label}: deleted chats are not listed"
        );
        for (kind, event) in quiet_kinds() {
            at += 1;
            h.apply(vec![BackendEvent::Message(notice(&d_alice(), at, event))])
                .unwrap_or_else(|e| panic!("{label} {kind}: {e}"));
            assert!(
                order(&h).is_empty(),
                "{label}: a {kind} notice brought the deleted chat back"
            );
        }
        assert_eq!(
            count(&h, "SELECT count(*) FROM messages WHERE quiet = 1"),
            4,
            "{label}: the notices are stored in the deleted chat"
        );
        // A loud notice and a text bring it back, as today.
        h.apply(vec![BackendEvent::Message(notice(
            &d_alice(),
            900,
            SystemEvent::TimerChanged {
                by: Some(alice()),
                timer: None,
            },
        ))])
        .expect("a loud notice");
        assert_eq!(order(&h), ["alice"], "{label}: a loud row brings it back");
        h.connection()
            .execute("UPDATE conversations SET hidden = 1", [])
            .expect("deleted again");
        h.apply(vec![BackendEvent::Message(text(&d_alice(), 950))])
            .expect("a text");
        assert_eq!(order(&h), ["alice"], "{label}: a text brings it back");
    }
}

/// A quiet notice is never unread and never notifies, live or in a history batch, whatever
/// the setting and whatever flags the backend put on it (today's rule, pinned: a notice has
/// no read state, so it is in no counter, no notification and no catch-up summary).
#[test]
fn a_quiet_notice_is_never_unread_and_never_notifies() {
    for shown in [false, true] {
        let key = d_alice();
        let (_dir, mut h) = scene(&key);
        h.set_quiet_notices(shown).expect("the setting");
        let before = row_of(&h, "alice");
        let mut at = 300;
        for (kind, event) in quiet_kinds() {
            // Live: not silent, not read, mentioning us, in the catch-up and out of it.
            for catch_up in [false, true] {
                at += 1;
                let mut m = notice(&key, at, event.clone());
                m.silent = false;
                m.read = false;
                m.mentions_me = true;
                m.catch_up = catch_up;
                let announced = h
                    .apply_announcing(vec![BackendEvent::Message(m)])
                    .unwrap_or_else(|e| panic!("{kind}: {e}"));
                assert!(announced.notify.is_empty(), "{kind}: a notification");
                assert_eq!(announced.catch_up, 0, "{kind}: a catch-up entry");
            }
            // History: the backend says "not read".
            at += 1;
            let mut m = notice(&key, at, event);
            m.read = false;
            m.mentions_me = true;
            let announced = h
                .apply_announcing(vec![history(&key, vec![m])])
                .unwrap_or_else(|e| panic!("{kind} (history): {e}"));
            assert!(announced.notify.is_empty(), "{kind}: a notification");
            assert_eq!(announced.catch_up, 0, "{kind}: a catch-up entry");
        }
        let after = row_of(&h, "alice");
        assert_eq!(after.unread, before.unread, "{shown}: the unread count");
        assert_eq!(after.mentions, before.mentions, "{shown}: the mentions");
        assert_eq!(
            count(
                &h,
                "SELECT count(*) FROM messages WHERE quiet = 1 AND read_state IS NOT NULL"
            ),
            0,
            "{shown}: a notice has a read state"
        );
        assert_eq!(
            count(&h, "SELECT count(*) FROM messages WHERE quiet = 1"),
            12,
            "{shown}: every notice is a row of the chat"
        );
    }
}

/// MEASUREMENT, run by hand (the numbers go into the build report; no assertion except the
/// answers being right):
///
/// ```text
/// systemd-run --user --scope -q -p MemoryMax=4G -p MemorySwapMax=0 timeout 900 \
///   cargo nextest run --workspace --exclude kourier-qt --run-ignored only \
///   --no-capture -E 'test(quiet_rows_at_the_top)'
/// ```
///
/// What the newest-listed-row walks cost when quiet rows are on top: (1) one chat of 20,000
/// messages with 1,000 quiet notices on top: one `recount` and the one-row list (its
/// preview), hidden (the walk steps over 1,000 rows) and shown (it stops at the first);
/// (2) the list page of 200 chats that each end in 3 quiet notices (the start path).
#[test]
#[ignore = "a measurement, run by hand"]
fn measure_quiet_rows_at_the_top_of_a_long_chat() {
    use std::time::Instant;

    let (_dir, mut h) = harness();
    let key = d_alice();
    for chunk in 0..20 {
        // (read: the two counts of a recount are then trivial and the numbers below are the
        // walk past the quiet rows, not the count of 20,000 unread rows)
        let events = (0..1_000)
            .map(|i| {
                let mut m = text(&key, 1_000 + chunk * 1_000 + i);
                m.read = true;
                BackendEvent::Message(m)
            })
            .collect();
        h.apply(events).expect("texts");
    }
    let events = (0..1_000)
        .map(|i| {
            BackendEvent::Message(notice(
                &key,
                100_000 + i,
                SystemEvent::IdentityKeyChanged { user: alice() },
            ))
        })
        .collect();
    h.apply(events).expect("1,000 notices on top");
    let alice_id = row_of(&h, "alice").id;
    let one_row = ConversationListQuery::default();
    for shown in [false, true] {
        h.set_quiet_notices(shown).expect("setting");
        let t = Instant::now();
        for _ in 0..200 {
            h.recount(alice_id).expect("recount");
        }
        let recount = t.elapsed() / 200;
        let t = Instant::now();
        for _ in 0..200 {
            h.conversations(&one_row).expect("list");
        }
        let list = t.elapsed() / 200;
        println!(
            "20,000 messages + 1,000 quiet on top, quiet notices {}: recount {recount:?}, \
             one-row list (preview) {list:?}",
            if shown { "shown" } else { "hidden" }
        );
    }

    // 200 chats, each with 5 texts and 3 quiet notices on top.
    let (_dir, mut h) = harness();
    let mut events = Vec::new();
    for n in 0..200_i64 {
        let peer = uid(&format!("aci:00000000-0000-4000-8000-{:012x}", 0x1000 + n));
        let conv = ConvKey::Direct(peer.clone());
        for k in 0..5 {
            events.push(BackendEvent::Message(remote(
                &conv,
                &peer,
                &format!("t-{n}-{k}"),
                10_000 + n * 100 + k,
                "hello",
            )));
        }
        for k in 0..3 {
            // (an id per `at`, and every `at` is its own)
            events.push(BackendEvent::Message(notice(
                &conv,
                10_000 + n * 100 + 50 + k,
                SystemEvent::IdentityKeyChanged { user: peer.clone() },
            )));
        }
    }
    h.apply(events).expect("200 chats");
    for shown in [false, true] {
        h.set_quiet_notices(shown).expect("setting");
        let t = Instant::now();
        let mut rows = 0;
        for _ in 0..20 {
            rows = h.conversations(&one_row).expect("list").rows.len();
        }
        println!(
            "list of {rows} chats that end in 3 quiet notices, quiet notices {}: {:?} a page",
            if shown { "shown" } else { "hidden" },
            t.elapsed() / 20
        );
    }
}
