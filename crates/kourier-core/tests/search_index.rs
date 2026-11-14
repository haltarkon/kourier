//! The writes of the search index (G1b): every path that writes `messages` reaches
//! `messages_fts` through the triggers and the flush. Every assertion looks at the INDEX ITSELF
//! (`testing::search`: a raw FTS5 `MATCH` and the `%_docsize` rows, critic H2): `Query::Search`
//! hides a deleted or view-once row by its own rules whether or not the index was touched.
//! The text is synthetic; each word is one no other row holds.

use crate::support;

use std::time::Duration;

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_core::facade::{Query, QueryData, UiCommand};
use kourier_core::testing::{IngestHarness, search};
use kourier_fake::scripts::{T0, own, remote, target};
use kourier_fake::{Flavor, Script};
use kourier_model::{
    Attachment, AttachmentKind, Body, ConvKey, ConversationId, MessageId, MsgIdScope, NetworkKind,
    Placeholder, SystemEvent, Text, UserId,
};

use support::{Env, WAIT, add_linked, eventually};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
const CAROL: &str = "aci:00000000-0000-4000-8000-0000000ca202";
const CAROL_PNI: &str = "pni:00000000-0000-4000-8000-0000000ca201";

/// The three ways a network names its messages: Signal's, Threema's, WhatsApp's.
const FLAVOURS: [(NetworkKind, MsgIdScope); 3] = [
    (NetworkKind::Fake, MsgIdScope::AccountAuthor),
    (NetworkKind::Fake, MsgIdScope::Conversation),
    (NetworkKind::Fake, MsgIdScope::ConversationAuthor),
];

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn alice() -> UserId {
    uid(ALICE)
}

fn d_alice() -> ConvKey {
    ConvKey::Direct(alice())
}

fn harness(dir: &tempfile::TempDir, (network, scope): (NetworkKind, MsgIdScope)) -> IngestHarness {
    IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([4; 32]),
        network,
        scope,
        &[uid(ME)],
    )
    .expect("harness")
}

fn said(n: i64, text: &str) -> BackendEvent {
    BackendEvent::Message(remote(
        &d_alice(),
        &alice(),
        &n.to_string(),
        T0 + n * 1000,
        text,
    ))
}

fn msg_id(h: &IngestHarness, local: &str) -> MessageId {
    h.connection()
        .query_row(
            "SELECT id FROM messages WHERE net_local = ?1",
            [local],
            |r| r.get(0).map(MessageId),
        )
        .expect("message")
}

fn conv_of(h: &IngestHarness, key: &ConvKey) -> ConversationId {
    h.connection()
        .query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [key.encode()],
            |r| r.get(0).map(ConversationId),
        )
        .expect("conversation")
}

fn found(h: &IngestHarness, word: &str) -> Vec<MessageId> {
    search::ids_with(h.search_db(), word).expect("raw match")
}

fn indexed(h: &IngestHarness) -> Vec<MessageId> {
    search::indexed(h.search_db()).expect("docsize")
}

fn flush(h: &mut IngestHarness) -> search::Flushed {
    let done = h.search_flush().expect("flush");
    assert_eq!(search::queued(h.connection()).expect("queue"), 0);
    search::assert_map_holds(h.search_db()).expect("the map");
    done
}

fn dead(h: &IngestHarness) -> i64 {
    search::meta(h.search_db(), "dead").expect("dead")
}

fn media(n: i64, caption: &str, name: &str) -> IncomingMessage {
    let mut m = remote(&d_alice(), &alice(), &n.to_string(), T0 + n * 1000, "");
    m.body = Body::Media {
        attachments: vec![Attachment {
            kind: AttachmentKind::File,
            mime: "application/octet-stream".into(),
            size: 10,
            file_name: Some(name.into()),
            ..Attachment::default()
        }],
        caption: Some(Text::plain(caption)),
    };
    m
}

#[test]
fn a_message_is_found_after_a_live_ingest_a_history_batch_and_from_another_device() {
    for flavour in FLAVOURS {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(&dir, flavour);
        let mut history = remote(&d_alice(), &alice(), "h1", T0 - 86_400_000, "betahistq");
        history.read = true;
        h.apply(vec![
            said(1, "alphaliveq and more"),
            BackendEvent::HistoryBatch {
                conv: d_alice(),
                messages: vec![history],
                has_more: false,
                progress: None,
            },
            BackendEvent::Message(own(&d_alice(), "o1", T0 + 5000, "gammaownq")),
            BackendEvent::Message(media(6, "deltacaptionq", "epsilonfileq.pdf")),
        ])
        .expect("apply");
        // Nothing is in the index before the flush: the queue holds the ids.
        assert!(indexed(&h).is_empty());
        assert_eq!(search::queued(h.connection()).expect("queue"), 4);
        let done = flush(&mut h);
        assert_eq!((done.taken, done.indexed, done.removed), (4, 4, 0));
        assert_eq!(found(&h, "alphaliveq"), [msg_id(&h, "1")]);
        assert_eq!(found(&h, "betahistq"), [msg_id(&h, "h1")]);
        assert_eq!(found(&h, "gammaownq"), [msg_id(&h, "o1")]);
        assert_eq!(found(&h, "deltacaptionq"), [msg_id(&h, "6")]);
        assert_eq!(found(&h, "epsilonfileq"), [msg_id(&h, "6")]);
        // Under the chat's own token, and no other.
        let a = conv_of(&h, &d_alice());
        assert_eq!(
            search::ids_with_in(h.search_db(), "alphaliveq", a).expect("raw"),
            [msg_id(&h, "1")]
        );
        assert!(
            search::ids_with_in(h.search_db(), "alphaliveq", ConversationId(a.0 + 1))
                .expect("raw")
                .is_empty()
        );
        // The rowid holds the hour of the message.
        let (sort_key, id): (i64, i64) = h
            .connection()
            .query_row(
                "SELECT sort_key, id FROM messages WHERE net_local = 'h1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("row");
        let rowid = search::rowid_of(sort_key, MessageId(id)).expect("rowid");
        let there: i64 = h
            .search_db()
            .query_row(
                "SELECT count(*) FROM messages_fts_docsize WHERE id = ?1",
                [rowid],
                |r| r.get(0),
            )
            .expect("docsize");
        assert_eq!(there, 1);
    }
}

#[test]
fn an_edit_takes_the_old_words_out_and_puts_the_new_ones_in() {
    for flavour in FLAVOURS {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(&dir, flavour);
        h.apply(vec![said(1, "oldformq")]).expect("apply");
        flush(&mut h);
        assert_eq!(found(&h, "oldformq"), [msg_id(&h, "1")]);
        h.apply(vec![BackendEvent::Edited {
            target: target(Some(&d_alice()), Some(&alice()), "1"),
            by: Some(alice()),
            new_local: Some("10".into()),
            body: Body::text("newformq"),
            edited_at: T0 + 10_000,
            keeps_quote: None,
        }])
        .expect("edit");
        let done = flush(&mut h);
        assert_eq!((done.removed, done.indexed), (1, 1));
        assert!(found(&h, "oldformq").is_empty(), "the old word is gone");
        assert_eq!(found(&h, "newformq"), [msg_id(&h, "1")]);
        assert_eq!(indexed(&h).len(), 1, "one entry, not two");
        assert_eq!(dead(&h), 1);
    }
}

/// Indexes three messages, then `act` must take the middle one's word out of the index.
fn gone_after(what: &str, act: impl Fn(&mut IngestHarness)) {
    for flavour in FLAVOURS {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(&dir, flavour);
        h.apply(vec![
            said(1, "keepfirstq"),
            said(2, "goneword"),
            said(3, "keeplastq"),
        ])
        .expect("apply");
        flush(&mut h);
        assert_eq!(found(&h, "goneword").len(), 1, "{what}: indexed first");
        act(&mut h);
        flush(&mut h);
        assert!(found(&h, "goneword").is_empty(), "{what}: the word is gone");
        let ids: Vec<i64> = h
            .connection()
            .prepare("SELECT id FROM messages")
            .expect("prepare")
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("ids");
        for m in indexed(&h) {
            assert!(ids.contains(&m.0), "{what}: no entry outlives its row");
        }
    }
}

#[test]
fn a_delete_for_everyone_takes_the_words_out() {
    gone_after("delete for everyone", |h| {
        h.apply(vec![BackendEvent::Deleted {
            target: target(Some(&d_alice()), Some(&alice()), "2"),
            by: Some(alice()),
            for_everyone: true,
            at: T0 + 20_000,
        }])
        .expect("delete");
    });
}

#[test]
fn a_delete_for_me_from_another_device_takes_the_words_out() {
    gone_after("delete for me", |h| {
        h.apply(vec![BackendEvent::Deleted {
            target: target(Some(&d_alice()), Some(&alice()), "2"),
            by: None,
            for_everyone: false,
            at: T0 + 20_000,
        }])
        .expect("delete for me");
    });
}

#[test]
fn a_clear_and_a_chat_delete_take_the_words_out() {
    gone_after("clear", |h| {
        let a = conv_of(h, &d_alice());
        h.clear_conversation(a, false).expect("clear");
    });
    gone_after("chat delete", |h| {
        let a = conv_of(h, &d_alice());
        h.clear_conversation(a, true).expect("delete the chat");
    });
    gone_after("chat delete on the phone", |h| {
        h.apply(vec![BackendEvent::ConversationCleared {
            conv: d_alice(),
            up_to: vec![target(Some(&d_alice()), Some(&alice()), "3")],
            remove: true,
            before_ms: None,
            at: T0 + 99_000,
        }])
        .expect("cleared");
    });
}

/// A clear and a chat's delete ask for a compaction in their own transaction, AFTER their
/// deletes: the ask covers the log rows the deletes appended, so the compaction that answers
/// it comes once the flush took them (their words leave within minutes, not a day).
#[test]
fn a_clear_asks_for_a_compaction_that_covers_its_deletes() {
    for remove in [false, true] {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(&dir, FLAVOURS[0]);
        h.apply(vec![said(1, "askoneq"), said(2, "asktwoq")])
            .expect("apply");
        flush(&mut h);
        let (one, two) = (msg_id(&h, "1"), msg_id(&h, "2"));
        let a = conv_of(&h, &d_alice());
        h.clear_conversation(a, remove).expect("clear");
        let deletes: i64 = h
            .connection()
            .query_row(
                "SELECT max(seq) FROM search_queue WHERE message_id IN (?1, ?2)",
                [one.0, two.0],
                |r| r.get(0),
            )
            .expect("the deletes' log rows");
        let asked = search::compact_after(h.connection()).expect("ask");
        assert!(
            asked >= deletes,
            "remove {remove}: the ask ({asked}) covers the deletes (up to {deletes})"
        );
        let done = flush(&mut h);
        assert_eq!(done.removed, 2);
        assert_eq!(
            search::due(h.search_db(), asked, T0).expect("due"),
            search::Due::Now,
            "remove {remove}"
        );
    }
}

#[test]
fn a_cascade_takes_the_words_out() {
    // An account's removal and a chat's removal delete rows through ON DELETE CASCADE, with no
    // statement of their own on `messages`: the delete trigger fires all the same.
    gone_after("account removal", |h| {
        let account = h.account();
        h.connection()
            .execute("DELETE FROM accounts WHERE id = ?1", [account.0])
            .expect("remove the account");
    });
    gone_after("chat row removed", |h| {
        let a = conv_of(h, &d_alice());
        h.connection()
            .execute("DELETE FROM conversations WHERE id = ?1", [a.0])
            .expect("remove the chat");
    });
}

#[test]
fn an_expired_message_takes_its_words_out() {
    for flavour in FLAVOURS {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(&dir, flavour);
        let BackendEvent::Message(mut m) = said(1, "expiringq") else {
            unreachable!()
        };
        m.expiry = Some(kourier_model::Expiry {
            kind: kourier_model::DisappearKind::AfterSend,
            timer: Duration::from_secs(60),
            started_at: Some(T0 + 1000),
        });
        h.apply(vec![BackendEvent::Message(m), said(2, "stayingq")])
            .expect("apply");
        flush(&mut h);
        assert_eq!(
            found(&h, "expiringq").len(),
            1,
            "disappearing messages are found"
        );
        h.set_now(T0 + 1000 + 61_000);
        h.sweep().expect("sweep");
        flush(&mut h);
        assert!(found(&h, "expiringq").is_empty());
        assert_eq!(found(&h, "stayingq").len(), 1);
    }
}

#[test]
fn a_flag_or_a_chat_that_changes_alone_reaches_the_index() {
    // A writer that sets only the delete flag, or only the view-once flag (none does today:
    // the tombstone empties the body too), or moves a row to another chat.
    for flavour in FLAVOURS {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(&dir, flavour);
        h.apply(vec![
            said(1, "flaggedq"),
            said(2, "onceq"),
            said(3, "movedq"),
        ])
        .expect("apply");
        flush(&mut h);
        let c = h.connection();
        c.execute(
            "UPDATE messages SET deleted = 'ForEveryone' WHERE id = ?1",
            [msg_id(&h, "1").0],
        )
        .expect("flag");
        c.execute(
            "UPDATE messages SET view_once = 1 WHERE id = ?1",
            [msg_id(&h, "2").0],
        )
        .expect("view once");
        let a = conv_of(&h, &d_alice());
        c.execute(
            "INSERT INTO conversations(account_id, conv_key, kind) VALUES (?1, 'aci:other', 'Direct')",
            [h.account().0],
        )
        .expect("another chat");
        let other = ConversationId(c.last_insert_rowid());
        c.execute(
            "UPDATE messages SET conversation_id = ?2 WHERE id = ?1",
            [msg_id(&h, "3").0, other.0],
        )
        .expect("move");
        flush(&mut h);
        assert!(found(&h, "flaggedq").is_empty(), "a deleted row is out");
        assert!(found(&h, "onceq").is_empty(), "a view-once row is out");
        let moved = msg_id(&h, "3");
        assert_eq!(
            search::ids_with_in(h.search_db(), "movedq", other).expect("raw"),
            [moved],
            "under the chat it is in now"
        );
        assert!(
            search::ids_with_in(h.search_db(), "movedq", a)
                .expect("raw")
                .is_empty(),
            "not under the chat it left"
        );
    }
}

#[test]
fn a_merge_of_two_chats_files_the_words_under_the_chat_that_stays() {
    let (carol, carol_pni) = (uid(CAROL), uid(CAROL_PNI));
    let (by_aci, by_pni) = (
        ConvKey::Direct(carol.clone()),
        ConvKey::Direct(carol_pni.clone()),
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, FLAVOURS[0]);
    h.apply(vec![
        BackendEvent::Message(remote(&by_aci, &carol, "c1", T0 + 1000, "underaciq")),
        BackendEvent::Message(remote(&by_pni, &carol_pni, "c2", T0 + 2000, "underpniq")),
    ])
    .expect("apply");
    flush(&mut h);
    let (into, from) = (conv_of(&h, &by_aci), conv_of(&h, &by_pni));
    assert_eq!(
        search::ids_with_in(h.search_db(), "underpniq", from)
            .expect("raw")
            .len(),
        1
    );
    h.apply(vec![BackendEvent::IdentityMerged {
        from: carol_pni,
        into: carol,
    }])
    .expect("merge");
    flush(&mut h);
    let row = msg_id(&h, "c2");
    assert_eq!(
        search::ids_with_in(h.search_db(), "underpniq", into).expect("raw"),
        [row],
        "found under the chat that stays"
    );
    assert!(
        search::ids_with_in(h.search_db(), "underpniq", from)
            .expect("raw")
            .is_empty(),
        "not under the chat that went"
    );
}

#[test]
fn the_kinds_that_are_never_searched_stay_out_of_the_index() {
    for flavour in FLAVOURS {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(&dir, flavour);
        let mut once = media(1, "viewonceq caption", "viewoncefileq.jpg");
        once.view_once = true;
        let mut notice = remote(&d_alice(), &alice(), "2", T0 + 2000, "");
        notice.body = Body::System(SystemEvent::TitleChanged {
            by: Some(alice()),
            title: "noticetitleq".into(),
        });
        let mut sticker = remote(&d_alice(), &alice(), "3", T0 + 3000, "");
        sticker.body = Body::Sticker(Box::new(Attachment {
            kind: AttachmentKind::Image,
            mime: "image/webp".into(),
            caption: Some(Text::plain("stickerwordq")),
            file_name: Some("stickerfileq.webp".into()),
            ..Attachment::default()
        }));
        let mut placeholder = remote(&d_alice(), &alice(), "4", T0 + 4000, "");
        placeholder.body = Body::Placeholder(Placeholder::Expired);
        h.apply(vec![
            BackendEvent::Message(once),
            BackendEvent::Message(notice),
            BackendEvent::Message(sticker),
            BackendEvent::Message(placeholder),
            said(5, "plainwordq"),
        ])
        .expect("apply");
        let done = flush(&mut h);
        assert_eq!(done.indexed, 1, "only the text");
        for word in [
            "viewonceq",
            "viewoncefileq",
            "noticetitleq",
            "stickerwordq",
            "stickerfileq",
        ] {
            assert!(found(&h, word).is_empty(), "{word} must not be indexed");
        }
        assert_eq!(indexed(&h), [msg_id(&h, "5")]);
        // Their deletes write no tombstone: there is nothing of them in the index.
        let before = dead(&h);
        h.connection()
            .execute(
                "DELETE FROM messages WHERE net_local IN ('1', '2', '3', '4')",
                [],
            )
            .expect("delete");
        let done = flush(&mut h);
        assert_eq!(done.removed, 0);
        assert_eq!(dead(&h), before, "only deletes that found an entry count");
    }
}

#[test]
fn a_shuffled_batch_of_500_cuts_one_segment() {
    // History rows carry their sent time, so their rowids (week first) come in any order
    // while their ids rise: the flush sorts them, else FTS5 cuts a segment at every rowid
    // that goes down.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, FLAVOURS[0]);
    let week = 7 * 86_400_000;
    let mut events = Vec::new();
    let mut batch = Vec::new();
    for n in 0..500_i64 {
        // A spread over 97 weeks in an order that jumps back and forth.
        let weeks_back = (n * 37) % 97;
        let mut m = remote(
            &d_alice(),
            &alice(),
            &format!("s{n}"),
            T0 - weeks_back * week - n,
            &format!("word{n} shuffledq"),
        );
        m.read = true;
        batch.push(m);
        if batch.len() == 100 {
            events.push(BackendEvent::HistoryBatch {
                conv: d_alice(),
                messages: std::mem::take(&mut batch),
                has_more: true,
                progress: None,
            });
        }
    }
    h.apply(events).expect("apply");
    assert_eq!(search::queued(h.connection()).expect("queue"), 500);
    // A batch larger than the log: one flush takes all of it and knows nothing is left.
    let done = h.search_flush_one(501).expect("one flush");
    assert_eq!((done.taken, done.indexed, done.more), (500, 500, false));
    assert_eq!(found(&h, "shuffledq").len(), 500);
    assert_eq!(
        search::segments(h.search_db()).expect("segments"),
        1,
        "one batch, one segment"
    );
}

#[test]
fn a_body_that_does_not_parse_is_not_searchable_and_the_batch_goes_on() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, FLAVOURS[0]);
    h.apply(vec![
        said(1, "beforepoisonq"),
        said(2, "poisonq"),
        said(3, "afterpoisonq"),
    ])
    .expect("apply");
    // A body that is not a `Body` (old fixtures hold such, `'{}'`).
    h.connection()
        .execute(
            "UPDATE messages SET body = '{}' WHERE id = ?1",
            [msg_id(&h, "2").0],
        )
        .expect("poison");
    let done = flush(&mut h);
    // Four log rows (the update of row 2 is logged again; the log is append-only), three ids.
    assert_eq!((done.taken, done.indexed, done.unreadable), (4, 2, 1));
    assert_eq!(found(&h, "beforepoisonq").len(), 1);
    assert_eq!(found(&h, "afterpoisonq").len(), 1);
    assert!(found(&h, "poisonq").is_empty());
}

#[test]
fn rows_at_or_below_the_floor_wait_for_the_backfill() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, FLAVOURS[0]);
    h.apply(vec![said(1, "belowfloorq"), said(2, "abovefloorq")])
        .expect("apply");
    // As a file made over rows that were there leaves them: they wait below `floor`, and the
    // flush leaves a log row of theirs to the backfill (the FLOOR SKIP).
    search::set_meta(h.search_db(), "floor", msg_id(&h, "1").0).expect("floor");
    let done = flush(&mut h);
    assert_eq!((done.taken, done.indexed), (2, 1));
    assert!(
        found(&h, "belowfloorq").is_empty(),
        "the backfill reads it later"
    );
    assert_eq!(found(&h, "abovefloorq").len(), 1);
    // The backfill then indexes the row it was left: a row never indexed leaves no tombstone
    // (storage LOW-7: `dead` counts only deletes that found an entry).
    h.search_backfill_all(5).expect("backfill");
    assert_eq!(found(&h, "belowfloorq").len(), 1);
    assert_eq!(found(&h, "abovefloorq").len(), 1);
    assert_eq!(dead(&h), 0, "no delete found an entry");
}

/// THE PRUNE stops at what search.db committed: a change written to core.db after the flush
/// read its snapshot (and before search.db's commit) is a log row above `applied`; it stays
/// in the log and the next flush writes it.
#[test]
fn a_change_made_while_a_flush_runs_stays_in_the_log_for_the_next() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, FLAVOURS[0]);
    h.apply(vec![said(1, "beforeflushq")]).expect("apply");
    let id = msg_id(&h, "1");
    let done = h
        .search_flush_between(
            100,
            |core| {
                core.execute(
                    "UPDATE messages SET body = '{\"Text\":{\"body\":\"duringflushq\"}}'
                      WHERE id = ?1",
                    [id.0],
                )?;
                Ok(())
            },
            true,
        )
        .expect("flush");
    assert_eq!((done.taken, done.indexed), (1, 1));
    assert_eq!(
        found(&h, "beforeflushq"),
        [id],
        "the flush wrote its snapshot"
    );
    assert_eq!(
        search::queued(h.connection()).expect("queue"),
        1,
        "the change made meanwhile stays in the log"
    );
    let done = flush(&mut h);
    assert_eq!((done.taken, done.indexed, done.removed), (1, 1, 1));
    assert_eq!(found(&h, "duringflushq"), [id]);
    assert!(found(&h, "beforeflushq").is_empty());
}

/// Rewrites a row's text in core.db (a trigger of the log follows).
fn edit(h: &IngestHarness, id: MessageId, word: &str) {
    h.connection()
        .execute(
            "UPDATE messages SET body = ?2 WHERE id = ?1",
            (id.0, format!("{{\"Text\":{{\"body\":\"{word}\"}}}}")),
        )
        .expect("edit");
}

fn applied(h: &IngestHarness) -> i64 {
    search::meta(h.search_db(), "applied").expect("applied")
}

/// A POISON ROW (storage LOW-6, review P2 + D2): a row the index will not take (here a trigger
/// of the test's own refuses its `indexed` line, an error that is no damage). THE FIRST FAILURE of
/// the batch is the job's error: nothing is skipped, nothing moves, the log stays. The SECOND
/// try (the same batch) goes id by id: the failing id is skipped, the others are written,
/// `applied` moves on; the skipped id's OLD words are gone with its take-out (it is not
/// searchable until its next change) and the take-out is counted. When EVERY id fails it is
/// the file, not a row: the second try fails too, nothing is skipped and the log stays.
#[test]
fn a_row_the_index_will_not_take_is_skipped_on_the_second_try_and_loses_its_old_words() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, FLAVOURS[0]);
    h.apply(vec![said(1, "oneq"), said(2, "twoq"), said(3, "threeq")])
        .expect("apply");
    flush(&mut h);
    let (one, two, three) = (msg_id(&h, "1"), msg_id(&h, "2"), msg_id(&h, "3"));
    assert_eq!(dead(&h), 0);
    h.search_db()
        .execute_batch(&format!(
            "CREATE TRIGGER poison BEFORE INSERT ON indexed WHEN new.message_id = {}
               BEGIN SELECT RAISE(ABORT, 'poison'); END;",
            two.0
        ))
        .expect("the poison");
    edit(&h, one, "onenewq");
    edit(&h, two, "twonewq");
    edit(&h, three, "threenewq");
    h.apply(vec![said(4, "fourq")]).expect("apply");
    let four = msg_id(&h, "4");
    let before = applied(&h);
    // The first try fails whole.
    h.search_flush()
        .expect_err("the first failure of a batch is the job's error");
    assert_eq!(applied(&h), before, "nothing moved");
    assert_eq!(search::queued(h.connection()).expect("queue"), 4);
    assert_eq!(found(&h, "twoq"), [two], "nothing was taken out");
    // The second try goes id by id.
    let done = h.search_flush_retry().expect("the second try");
    assert_eq!(
        (done.taken, done.indexed, done.removed, done.skipped),
        (4, 3, 3, 1),
        "the poisoned id is skipped, the others written, every take-out counted"
    );
    assert_eq!(dead(&h), 3, "the skipped id's take-out stays");
    assert_eq!(found(&h, "onenewq"), [one]);
    assert_eq!(found(&h, "threenewq"), [three]);
    assert_eq!(found(&h, "fourq"), [four]);
    assert!(found(&h, "twonewq").is_empty(), "not searchable");
    assert!(
        found(&h, "twoq").is_empty(),
        "a skipped id never keeps its old words"
    );
    assert_eq!(applied(&h), done.applied);
    assert_eq!(search::queued(h.connection()).expect("queue"), 0);
    search::integrity_check(h.search_db()).expect("integrity-check");
    // Its next change makes it searchable again.
    h.search_db()
        .execute_batch("DROP TRIGGER poison")
        .expect("cure");
    edit(&h, two, "twoagainq");
    let done = flush(&mut h);
    assert_eq!((done.taken, done.indexed, done.skipped), (1, 1, 0));
    assert_eq!(found(&h, "twoagainq"), [two]);
    // Every id fails: the file, not a row.
    h.search_db()
        .execute_batch(
            "CREATE TRIGGER poison BEFORE INSERT ON indexed BEGIN SELECT RAISE(ABORT, 'all'); END;",
        )
        .expect("poison all");
    let before = applied(&h);
    edit(&h, one, "oneagainq");
    h.apply(vec![said(5, "fiveq")]).expect("apply");
    h.search_flush().expect_err("the first try fails");
    h.search_flush_retry()
        .expect_err("every id fails: the second try fails too");
    assert_eq!(applied(&h), before, "nothing is skipped");
    assert_eq!(
        search::queued(h.connection()).expect("queue"),
        2,
        "the log stays"
    );
    h.search_db()
        .execute_batch("DROP TRIGGER poison")
        .expect("cure");
    let done = flush(&mut h);
    assert_eq!((done.taken, done.indexed, done.skipped), (2, 2, 0));
    assert_eq!(found(&h, "oneagainq"), [one]);
    assert_eq!(found(&h, "fiveq"), [msg_id(&h, "5")]);
}

/// A row the index will not take, ALONE in its batch (a new message is usually flushed alone):
/// the second try tells by a throwaway insert that the file is fine and the id is a row, so
/// the id is skipped (not searchable, its old words gone) and `applied` moves on; the keep
/// loop would otherwise count three failures and build a healthy file again (re-check NEW).
#[test]
fn a_row_the_index_will_not_take_is_skipped_even_alone_in_its_batch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, FLAVOURS[0]);
    h.apply(vec![said(1, "oneq"), said(2, "twoq")])
        .expect("apply");
    flush(&mut h);
    let (one, two) = (msg_id(&h, "1"), msg_id(&h, "2"));
    h.search_db()
        .execute_batch(&format!(
            "CREATE TRIGGER poison BEFORE INSERT ON indexed WHEN new.message_id = {}
               BEGIN SELECT RAISE(ABORT, 'poison'); END;",
            two.0
        ))
        .expect("the poison");
    edit(&h, two, "twonewq");
    h.search_flush().expect_err("the first try fails whole");
    let done = h
        .search_flush_retry()
        .expect("the second try skips the row");
    assert_eq!((done.taken, done.indexed, done.skipped), (1, 0, 1));
    assert!(found(&h, "twonewq").is_empty(), "not searchable");
    assert!(found(&h, "twoq").is_empty(), "its old words are gone");
    assert_eq!(found(&h, "oneq"), [one], "the others stay");
    assert_eq!(search::queued(h.connection()).expect("queue"), 0);
    search::integrity_check(h.search_db()).expect("integrity-check");
}

/// THE BACKFILL's poison pass (re-check NEW): a row the index will not take is skipped and
/// counted, the other rows of its batch are indexed, and `floor` reaches 0 (the 24 hour
/// compaction rule waits for that).
#[test]
fn a_row_the_backfill_cannot_index_is_skipped_and_the_floor_reaches_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, FLAVOURS[0]);
    h.apply((1..=30).map(|n| said(n, &format!("fill{n}q"))).collect())
        .expect("apply");
    let (bad_a, bad_b) = (msg_id(&h, "5"), msg_id(&h, "20"));
    search::set_meta(h.search_db(), "floor", msg_id(&h, "30").0).expect("floor");
    h.search_db()
        .execute_batch(&format!(
            "CREATE TRIGGER poison BEFORE INSERT ON indexed WHEN new.message_id IN ({}, {})
               BEGIN SELECT RAISE(ABORT, 'poison'); END;",
            bad_a.0, bad_b.0
        ))
        .expect("the poison");
    let done = h.search_backfill_all(5).expect("backfill");
    assert_eq!((done.floor, done.indexed, done.skipped), (0, 28, 2));
    assert_eq!(search::meta(h.search_db(), "floor").expect("floor"), 0);
    assert!(found(&h, "fill5q").is_empty());
    assert_eq!(found(&h, "fill6q").len(), 1);
    assert_eq!(found(&h, "fill19q").len(), 1);
    search::integrity_check(h.search_db()).expect("integrity-check");
}

/// A failure that stops after one try (an error that comes once) skips NOTHING: the first
/// failure is returned whole, and the same batch, tried again whole, writes every id (review
/// D2: a transient error must not lose a row for good).
#[test]
fn a_failure_that_comes_once_skips_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, FLAVOURS[0]);
    h.apply(vec![said(1, "oneq"), said(2, "twoq"), said(3, "threeq")])
        .expect("apply");
    flush(&mut h);
    let (one, two, three) = (msg_id(&h, "1"), msg_id(&h, "2"), msg_id(&h, "3"));
    h.search_db()
        .execute_batch(&format!(
            "CREATE TRIGGER once BEFORE INSERT ON indexed WHEN new.message_id = {}
               BEGIN SELECT RAISE(ABORT, 'once'); END;",
            two.0
        ))
        .expect("the failure");
    edit(&h, one, "onenewq");
    edit(&h, two, "twonewq");
    edit(&h, three, "threenewq");
    h.search_flush().expect_err("it fails once");
    // It is over (the trigger goes); the keep loop's back-off retry takes the whole batch.
    h.search_db()
        .execute_batch("DROP TRIGGER once")
        .expect("over");
    let done = flush(&mut h);
    assert_eq!((done.taken, done.indexed, done.skipped), (3, 3, 0));
    assert_eq!(found(&h, "onenewq"), [one]);
    assert_eq!(found(&h, "twonewq"), [two]);
    assert_eq!(found(&h, "threenewq"), [three]);
    assert!(found(&h, "twoq").is_empty());
}

/// THE DELETE VARIANT (review P2): a batch with a deleted message whose take-out fails is
/// never skipped, not even on the second try: the whole job fails, `applied` does not pass
/// the delete, the log keeps it, and its words leave once the failure stops.
#[test]
fn a_delete_whose_take_out_fails_is_never_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, FLAVOURS[0]);
    h.apply(vec![said(1, "goneq"), said(2, "stayq"), said(3, "otherq")])
        .expect("apply");
    flush(&mut h);
    let (gone, stay, other) = (msg_id(&h, "1"), msg_id(&h, "2"), msg_id(&h, "3"));
    h.search_db()
        .execute_batch(&format!(
            "CREATE TRIGGER poison BEFORE DELETE ON indexed WHEN old.message_id = {}
               BEGIN SELECT RAISE(ABORT, 'poison'); END;",
            gone.0
        ))
        .expect("the poison");
    let before = applied(&h);
    h.connection()
        .execute(
            "UPDATE messages SET deleted = 'ForEveryone' WHERE id = ?1",
            [gone.0],
        )
        .expect("delete for everyone");
    h.connection()
        .execute("DELETE FROM messages WHERE id = ?1", [other.0])
        .expect("delete");
    edit(&h, stay, "stayednewq");
    h.search_flush().expect_err("the first try fails");
    h.search_flush_retry()
        .expect_err("the take-out failure is not skipped on the second try");
    assert_eq!(applied(&h), before, "applied did not pass the delete");
    assert!(search::queued(h.connection()).expect("queue") >= 3);
    assert_eq!(found(&h, "goneq"), [gone], "its words are still there");
    h.search_db()
        .execute_batch("DROP TRIGGER poison")
        .expect("the failure stops");
    let done = flush(&mut h);
    assert_eq!(done.skipped, 0);
    assert!(found(&h, "goneq").is_empty(), "its words left");
    assert!(found(&h, "otherq").is_empty());
    assert_eq!(found(&h, "stayednewq"), [stay]);
    assert!(applied(&h) > before);
}

/// The log is APPEND-ONLY: every change of a row is a log row of its own, and a flush reads
/// the rows as they are at its snapshot, once per id. The old entry is found through the
/// `indexed` map whatever the row's sort key is now (a move in time changes the rowid), and a
/// row born, edited and deleted between two flushes leaves nothing (no entry, no tombstone).
#[test]
fn the_log_keeps_every_change_and_a_flush_writes_each_row_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, FLAVOURS[0]);
    h.apply(vec![said(1, "firstq")]).expect("apply");
    let id = msg_id(&h, "1");
    let logged = |h: &IngestHarness| search::log_seqs(h.connection()).expect("log").len();
    assert_eq!(logged(&h), 1, "a new row");
    h.connection()
        .execute(
            "UPDATE messages SET body = '{\"Text\":{\"body\":\"secondq\"}}' WHERE id = ?1",
            [id.0],
        )
        .expect("rewrite");
    assert_eq!(logged(&h), 2, "every change is a row of the log");
    let done = flush(&mut h);
    assert_eq!((done.taken, done.indexed, done.removed), (2, 1, 0));
    assert_eq!(found(&h, "secondq"), [id]);
    assert!(found(&h, "firstq").is_empty());
    let sort_key = |h: &IngestHarness| -> i64 {
        h.connection()
            .query_row("SELECT sort_key FROM messages WHERE id = ?1", [id.0], |r| {
                r.get(0)
            })
            .expect("sort key")
    };
    assert_eq!(
        search::indexed_map(h.search_db()).expect("map"),
        [(id, search::rowid_of(sort_key(&h), id).expect("rowid"))]
    );
    // A rewrite with the same body is no change of the index: no log row.
    h.connection()
        .execute(
            "UPDATE messages SET body = body, sort_key = sort_key WHERE id = ?1",
            [id.0],
        )
        .expect("same");
    assert_eq!(search::queued(h.connection()).expect("queue"), 0);
    // A move in time and a new text in one batch: the old entry goes by the map's rowid.
    h.connection()
        .execute(
            "UPDATE messages SET sort_key = sort_key + 100000000000000 WHERE id = ?1",
            [id.0],
        )
        .expect("move in time");
    h.connection()
        .execute(
            "UPDATE messages SET body = '{\"Text\":{\"body\":\"thirdq\"}}' WHERE id = ?1",
            [id.0],
        )
        .expect("rewrite");
    let before = dead(&h);
    let done = flush(&mut h);
    assert_eq!((done.taken, done.indexed, done.removed), (2, 1, 1));
    assert_eq!(dead(&h), before + 1, "the old entry left a tombstone");
    assert!(found(&h, "secondq").is_empty());
    assert_eq!(found(&h, "thirdq"), [id]);
    assert_eq!(indexed(&h), [id]);
    assert_eq!(
        search::indexed_map(h.search_db()).expect("map"),
        [(id, search::rowid_of(sort_key(&h), id).expect("rowid"))],
        "the map follows the move"
    );
    // Born, edited and deleted between two flushes: nothing of it reaches the index.
    h.apply(vec![said(2, "brieflyq")]).expect("apply");
    let brief = msg_id(&h, "2");
    h.connection()
        .execute(
            "UPDATE messages SET body = '{\"Text\":{\"body\":\"brieferq\"}}' WHERE id = ?1",
            [brief.0],
        )
        .expect("edit");
    h.connection()
        .execute("DELETE FROM messages WHERE id = ?1", [brief.0])
        .expect("delete");
    let before = dead(&h);
    let done = flush(&mut h);
    assert_eq!((done.taken, done.indexed, done.removed), (3, 0, 0));
    assert_eq!(dead(&h), before, "no entry, no tombstone");
    assert!(found(&h, "brieflyq").is_empty() && found(&h, "brieferq").is_empty());
    assert_eq!(indexed(&h), [id]);
}

/// Through the running core: search.db is not made before `UiReady`; then it is made with
/// the rows of the first start below its floor and the backfill reads them (after its time),
/// an own send reaches the index within moments through the log, and an account's removal (a
/// cascade) empties it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_running_core_keeps_the_index_up_with_its_writes() {
    let me = uid(ME);
    let script = Script::new("search", vec![me]).deliver(vec![said(1, "scriptwordq")]);
    let mut env = Env::new(Flavor::SignalLike, script).await;
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let queued = env.peek(|c| search::queued(c).expect("queue"));
    assert!(queued > 0, "the rows are in the log");
    assert!(!env.search_db().exists(), "no search.db before UiReady");
    env.handle.send(UiCommand::UiReady).expect("send");
    // search.db may exist a moment before its schema is committed: not yet readable = empty.
    let raw = |env: &Env, word: &'static str| {
        if !env.search_db().exists() {
            return Vec::new();
        }
        env.try_peek_search(|c| search::ids_with(c, word))
            .unwrap_or_default()
    };
    // The backfill waits its 10 s after `UiReady`.
    eventually("the first start's rows are backfilled", WAIT * 2, || {
        let n = raw(&env, "scriptwordq").len();
        async move { n == 1 }
    })
    .await;
    // `Query::SearchQueued` (what the perf seed waits for) counts the log: the first start's
    // rows were pruned once the index took them.
    let queued_rows = |env: &Env| {
        let handle = env.handle.clone();
        async move {
            match handle.query(Query::SearchQueued).await.expect("query") {
                QueryData::SearchQueued(n) => n,
                other => panic!("{other:?}"),
            }
        }
    };
    eventually("the log is empty", WAIT, || {
        let n = queued_rows(&env);
        async move { n.await == 0 }
    })
    .await;
    let conv: ConversationId = env.peek(|c| {
        c.query_row("SELECT id FROM conversations LIMIT 1", [], |r| {
            r.get(0).map(ConversationId)
        })
        .expect("chat")
    });
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain("ownsentwordq"),
            reply_to: None,
        })
        .expect("send");
    eventually("an own send is indexed", WAIT, || {
        let n = raw(&env, "ownsentwordq").len();
        async move { n == 1 }
    })
    .await;
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    eventually("the removal empties the index", WAIT, || {
        let n = env
            .peek_search(|c| search::indexed(c).expect("docsize"))
            .len();
        async move { n == 0 }
    })
    .await;
    assert!(raw(&env, "scriptwordq").is_empty());
    env.stop().await;
}
