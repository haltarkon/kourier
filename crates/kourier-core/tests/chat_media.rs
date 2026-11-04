//! People pages, step P4 (architecture §4.2, §8.3): `Query::ChatMedia`, the gallery of a chat
//! ("Media and Files"): its order and its pages, the two kinds, what it leaves out and why, and
//! (an ignored measurement, run by hand in a release build) what it costs in a chat of 50,000
//! texts.

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_core::facade::{ChatMediaPage, ChatMediaQuery, MediaCursor, MediaKind};
use kourier_core::testing::IngestHarness;
use kourier_db::rusqlite::Connection;
use kourier_fake::attachments::{synthetic_file, synthetic_image};
use kourier_model::{
    AskFailure, Attachment, AttachmentKind, AttachmentState, Body, ConvKey, ConversationId,
    MsgIdScope, NetworkKind, Origin, UserId,
};

const ME: &str = "aci:00000000-0000-4000-8000-00000000000a";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn harness(dir: &tempfile::TempDir) -> IngestHarness {
    IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([4; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness")
}

fn key() -> ConvKey {
    ConvKey::Direct(uid(ALICE))
}

/// A message of `body` from Alice at `at` (the network id is the time).
fn message(at: i64, body: Body) -> IncomingMessage {
    let mut m = IncomingMessage::new(
        key(),
        at.to_string().as_str(),
        Origin::Remote {
            author: uid(ALICE),
            device: Some(1),
        },
        body,
        at,
        at + 500,
    );
    m.read = true;
    m
}

fn media(at: i64, attachments: Vec<Attachment>) -> IncomingMessage {
    message(
        at,
        Body::Media {
            attachments,
            caption: None,
        },
    )
}

fn picture(seed: u64) -> Attachment {
    synthetic_image(60, 40, seed)
}

fn file(name: &str, seed: u64) -> Attachment {
    synthetic_file(name, "application/pdf", 1_000 + seed, seed)
}

fn of_kind(kind: AttachmentKind, mut a: Attachment) -> Attachment {
    a.kind = kind;
    a
}

fn conv_id(h: &IngestHarness) -> ConversationId {
    h.connection()
        .query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [key().encode()],
            |r| r.get::<_, i64>(0),
        )
        .map(ConversationId)
        .expect("conversation")
}

fn ingest(h: &mut IngestHarness, messages: Vec<IncomingMessage>) {
    h.apply(messages.into_iter().map(BackendEvent::Message).collect())
        .expect("apply");
}

fn ask(
    h: &IngestHarness,
    kind: MediaKind,
    before: Option<MediaCursor>,
    limit: u32,
) -> ChatMediaPage {
    h.chat_media(&ChatMediaQuery {
        conv: conv_id(h),
        kind,
        before,
        limit,
    })
    .expect("the gallery")
}

fn cursor_after(p: &ChatMediaPage) -> MediaCursor {
    let last = p.items.last().expect("an item to go on after");
    MediaCursor {
        sort_key: last.sort_key,
        message: last.message,
        idx: Some(last.idx),
    }
}

/// What a page holds, as (message time, place in the message): the time is the network id.
fn places(h: &IngestHarness, p: &ChatMediaPage) -> Vec<(i64, u32)> {
    p.items
        .iter()
        .map(|i| {
            let local: String = h
                .connection()
                .query_row(
                    "SELECT net_local FROM messages WHERE id = ?1",
                    [i.message.0],
                    |r| r.get(0),
                )
                .expect("message");
            (local.parse().expect("time"), i.idx)
        })
        .collect()
}

fn message_row(c: &Connection, at: i64) -> i64 {
    c.query_row(
        "SELECT id FROM messages WHERE net_local = ?1",
        [at.to_string()],
        |r| r.get(0),
    )
    .expect("message")
}

/// The gallery lists the newest message first and the attachments of one message in their order,
/// pages by a cursor that may stand INSIDE an album (the page ends between two pictures of one
/// message), and the pages put together are the one big page; `more` is true while any follow.
#[test]
fn the_gallery_lists_newest_first_and_pages_from_a_cursor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    ingest(
        &mut h,
        vec![
            media(1_000, vec![picture(1)]),
            media(2_000, vec![picture(2), picture(3), picture(4)]),
            message(3_000, Body::text("not media")),
            media(4_000, vec![of_kind(AttachmentKind::Video, picture(5))]),
            media(
                5_000,
                vec![
                    of_kind(AttachmentKind::Gif, picture(6)),
                    file("plan.pdf", 7),
                ],
            ),
        ],
    );
    let all = ask(&h, MediaKind::Visual, None, 50);
    assert_eq!(
        places(&h, &all),
        [
            (5_000, 0),
            (4_000, 0),
            (2_000, 0),
            (2_000, 1),
            (2_000, 2),
            (1_000, 0)
        ],
        "newest message first, an album in its order, the text and the file left out"
    );
    assert!(!all.more, "nothing follows the last one");
    assert!(all.items.iter().all(|i| i.attachment.id.is_some()));

    // Pages of two, the second one ending inside the album of three.
    let first = ask(&h, MediaKind::Visual, None, 2);
    assert_eq!(places(&h, &first), [(5_000, 0), (4_000, 0)]);
    assert!(first.more);
    let second = ask(&h, MediaKind::Visual, Some(cursor_after(&first)), 2);
    assert_eq!(places(&h, &second), [(2_000, 0), (2_000, 1)]);
    assert!(
        second.more,
        "the third picture of the album and one more follow"
    );
    let third = ask(&h, MediaKind::Visual, Some(cursor_after(&second)), 2);
    assert_eq!(
        places(&h, &third),
        [(2_000, 2), (1_000, 0)],
        "the page that goes on inside an album takes its other pictures first"
    );
    assert!(!third.more);
    let mut joined = places(&h, &first);
    joined.extend(places(&h, &second));
    joined.extend(places(&h, &third));
    assert_eq!(joined, places(&h, &all), "the pages are the whole");

    // An exact fit does not claim more.
    let exact = ask(&h, MediaKind::Visual, None, 6);
    assert_eq!(exact.items.len(), 6);
    assert!(!exact.more, "one row more than asked is what says more");
    // A limit of nothing is one (a page of none could never go on).
    assert_eq!(ask(&h, MediaKind::Visual, None, 0).items.len(), 1);
}

/// A page can start AT a message (what the picture viewer asks for, to read the whole album of the
/// picture it shows): `idx: None` includes the message's first attachment, a cursor after its
/// first one does not, and the older messages follow either way.
#[test]
fn a_page_can_start_at_a_message_and_take_its_whole_album() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    ingest(
        &mut h,
        vec![
            media(1_000, vec![picture(1)]),
            media(2_000, vec![picture(2), picture(3), picture(4)]),
            media(4_000, vec![of_kind(AttachmentKind::Video, picture(5))]),
        ],
    );
    let all = ask(&h, MediaKind::Visual, None, 50);
    let album = all
        .items
        .iter()
        .find(|i| i.idx == 2)
        .expect("the album's last");
    let at = MediaCursor {
        sort_key: album.sort_key,
        message: album.message,
        idx: None,
    };
    let from = ask(&h, MediaKind::Visual, Some(at), 50);
    assert_eq!(
        places(&h, &from),
        [(2_000, 0), (2_000, 1), (2_000, 2), (1_000, 0)],
        "the album from its first picture, then the older message; the newer one is not listed"
    );
    let after_first = MediaCursor { idx: Some(0), ..at };
    assert_eq!(
        places(&h, &ask(&h, MediaKind::Visual, Some(after_first), 50)),
        [(2_000, 1), (2_000, 2), (1_000, 0)],
        "a cursor after the first picture skips it"
    );
    // The album's size as the limit: all of it, and more follows (the older message).
    let whole = ask(&h, MediaKind::Visual, Some(at), 3);
    assert_eq!(places(&h, &whole), [(2_000, 0), (2_000, 1), (2_000, 2)]);
    assert!(whole.more);
}

/// "Media" is pictures, videos and GIFs, "Files" is files and audio files; a voice note is neither
/// and a sticker (its own body kind) is neither; a picture sent as a file is a file. The first
/// page says whether the other kind has any, a later page does not ask.
#[test]
fn the_two_kinds_are_told_apart_and_the_first_page_names_the_other() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let sticker = Body::Sticker(Box::new(picture(9)));
    ingest(
        &mut h,
        vec![
            media(1_000, vec![picture(1)]),
            media(2_000, vec![file("a.pdf", 2)]),
            media(
                3_000,
                vec![of_kind(AttachmentKind::Audio, file("song.mp3", 3))],
            ),
            media(
                4_000,
                vec![of_kind(AttachmentKind::Voice, file("v.ogg", 4))],
            ),
            message(5_000, sticker),
            media(
                6_000,
                vec![of_kind(AttachmentKind::File, synthetic_image(10, 10, 5))],
            ),
        ],
    );
    let visual = ask(&h, MediaKind::Visual, None, 50);
    assert_eq!(places(&h, &visual), [(1_000, 0)], "the picture alone");
    assert!(visual.other, "there are files");
    let files = ask(&h, MediaKind::Files, None, 50);
    assert_eq!(
        places(&h, &files),
        [(6_000, 0), (3_000, 0), (2_000, 0)],
        "the picture sent as a file, the audio file and the file; no voice note, no sticker"
    );
    assert!(files.other, "there is a picture");
    assert_eq!(files.items[0].attachment.kind, AttachmentKind::File);
    assert_eq!(files.items[1].attachment.kind, AttachmentKind::Audio);
    assert_eq!(
        files.items[2].attachment.file_name.as_deref(),
        Some("a.pdf"),
        "the file name comes with it"
    );

    // Only the first page asks about the other kind.
    let later = ask(
        &h,
        MediaKind::Files,
        Some(cursor_after(&ask(&h, MediaKind::Files, None, 1))),
        50,
    );
    assert_eq!(later.items.len(), 2);
    assert!(!later.other, "a later page does not ask");

    // A chat with only one kind says so, and one with none says nothing.
    let dir2 = tempfile::tempdir().expect("tempdir");
    let mut only = harness(&dir2);
    ingest(&mut only, vec![media(1_000, vec![picture(1)])]);
    assert!(!ask(&only, MediaKind::Visual, None, 50).other);
    let dir3 = tempfile::tempdir().expect("tempdir");
    let mut none = harness(&dir3);
    ingest(&mut none, vec![message(1_000, Body::text("hello"))]);
    for kind in [MediaKind::Visual, MediaKind::Files] {
        let p = ask(&none, kind, None, 50);
        assert!(p.items.is_empty() && !p.more && !p.other);
    }
}

/// What has nothing to show or open is left out: a message deleted for everyone or for me, a
/// view-once message, a message hidden because its disappearing time has run out on an
/// unconfirmed clock, the attachment that is the rest of a long text, and an attachment the
/// network no longer has or never gave this device ("Unavailable", "Expired", a phone that said
/// no). What is on its way stays: not downloaded, downloading, here, failed, asking the phone.
#[test]
fn what_has_nothing_to_show_is_left_out() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let mut once = media(2_000, vec![picture(2)]);
    once.view_once = true;
    ingest(
        &mut h,
        vec![
            media(1_000, vec![picture(1)]),
            once,
            media(3_000, vec![picture(3)]),
            media(4_000, vec![picture(4)]),
            media(5_000, vec![picture(5)]),
            media(6_000, vec![picture(6), picture(7)]),
            media(7_000, vec![picture(8)]),
            media(8_000, vec![picture(9)]),
            media(9_000, vec![picture(10)]),
            media(10_000, vec![picture(11)]),
            media(11_000, vec![picture(12)]),
            media(12_000, vec![picture(13)]),
            media(13_000, vec![picture(14)]),
        ],
    );
    let c = h.connection();
    let state = |at: i64, idx: i64, s: AttachmentState| {
        let json = serde_json::to_string(&s).expect("json");
        c.execute(
            "UPDATE attachments SET state = ?3 WHERE idx = ?2
               AND message_id = (SELECT id FROM messages WHERE net_local = ?1)",
            (at.to_string(), idx, json),
        )
        .expect("state");
    };
    let msg = |at: i64, sql: &str| {
        c.execute(sql, [message_row(c, at)]).expect("update");
    };
    msg(
        3_000,
        "UPDATE messages SET deleted = 'ForEveryone' WHERE id = ?1",
    );
    msg(4_000, "UPDATE messages SET deleted = 'ForMe' WHERE id = ?1");
    msg(5_000, "UPDATE messages SET expiry_hidden = 1 WHERE id = ?1");
    c.execute(
        "UPDATE attachments SET long_text = 1 WHERE idx = 1
           AND message_id = (SELECT id FROM messages WHERE net_local = '6000')",
        [],
    )
    .expect("long text");
    state(7_000, 0, AttachmentState::Unavailable);
    state(8_000, 0, AttachmentState::Expired);
    state(
        9_000,
        0,
        AttachmentState::AskFailed {
            why: AskFailure::NoAnswer,
        },
    );
    state(
        10_000,
        0,
        AttachmentState::Downloading { done: 1, total: 9 },
    );
    state(11_000, 0, AttachmentState::Failed { retryable: true });
    state(12_000, 0, AttachmentState::Asking);
    state(13_000, 0, AttachmentState::Local);

    let page = ask(&h, MediaKind::Visual, None, 50);
    assert_eq!(
        places(&h, &page),
        [
            (13_000, 0),
            (12_000, 0),
            (11_000, 0),
            (10_000, 0),
            (6_000, 0),
            (1_000, 0)
        ],
        "not deleted, view-once, hidden, the long text's file, unavailable, expired, a phone that said no"
    );
    // The same rules for the rows of "Files" and the "is there any" question.
    let c = h.connection();
    c.execute("UPDATE attachments SET kind = 'File'", [])
        .expect("kinds");
    let files = ask(&h, MediaKind::Files, None, 50);
    assert_eq!(files.items.len(), 6, "the same six as files");
    let none = ask(&h, MediaKind::Visual, None, 50);
    assert!(
        none.items.is_empty() && none.other,
        "and the other kind is told"
    );
    c.execute(
        "UPDATE attachments SET state = '\"Unavailable\"' WHERE state <> '\"Unavailable\"'",
        [],
    )
    .expect("gone");
    assert!(
        !ask(&h, MediaKind::Visual, None, 50).other,
        "files that cannot be had do not count as files"
    );
}

/// A gallery page stays inside its chat, and the question about a chat that does not exist is an
/// empty page, not an error.
#[test]
fn a_gallery_page_is_one_chats_own() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    ingest(&mut h, vec![media(1_000, vec![picture(1)])]);
    let mut other = IncomingMessage::new(
        ConvKey::Direct(uid("aci:00000000-0000-4000-8000-00000000b0b0")),
        "2000",
        Origin::Remote {
            author: uid("aci:00000000-0000-4000-8000-00000000b0b0"),
            device: Some(1),
        },
        Body::Media {
            attachments: vec![picture(2), picture(3)],
            caption: None,
        },
        2_000,
        2_500,
    );
    other.read = true;
    h.apply(vec![BackendEvent::Message(other)]).expect("apply");
    assert_eq!(ask(&h, MediaKind::Visual, None, 50).items.len(), 1);
    let gone = h
        .chat_media(&ChatMediaQuery {
            conv: ConversationId(9_999),
            kind: MediaKind::Visual,
            before: None,
            limit: 10,
        })
        .expect("an empty page");
    assert!(gone.items.is_empty() && !gone.more && !gone.other);
}

/// The two lists follow what the core writes and deletes (the flags of `0020_chat_media.sql` are
/// kept by triggers, so the ingest, a deleted attachment and a deleted message need no code of
/// their own): a message that has a picture and a file is in both; the file deleted with the
/// core's own `DeleteAttachment` leaves the Files list and the Visual page's `other`; a message
/// deleted for me leaves both; a chat with a sticker and nothing else has neither.
#[test]
fn the_gallery_follows_the_attachments_the_core_writes_and_deletes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    ingest(
        &mut h,
        vec![
            media(1_000, vec![picture(1), file("a.pdf", 2)]),
            media(2_000, vec![file("b.pdf", 3)]),
            message(3_000, Body::Sticker(Box::new(picture(4)))),
        ],
    );
    assert_eq!(
        places(&h, &ask(&h, MediaKind::Visual, None, 50)),
        [(1_000, 0)]
    );
    let files = ask(&h, MediaKind::Files, None, 50);
    assert_eq!(places(&h, &files), [(2_000, 0), (1_000, 1)]);

    // The file of the first message is deleted by the core: the picture stays, the file goes.
    let file_id = files.items[1].attachment.id.expect("its id");
    h.delete_attachment(file_id).expect("delete the attachment");
    assert_eq!(
        places(&h, &ask(&h, MediaKind::Files, None, 50)),
        [(2_000, 0)]
    );
    assert_eq!(
        places(&h, &ask(&h, MediaKind::Visual, None, 50)),
        [(1_000, 0)]
    );
    assert!(
        ask(&h, MediaKind::Visual, None, 50).other,
        "the second message's file"
    );

    // The second message is deleted for me: its file is gone from the list, and `other` with it.
    let c = h.connection();
    c.execute(
        "UPDATE messages SET deleted = 'ForMe' WHERE id = ?1",
        [message_row(c, 2_000)],
    )
    .expect("deleted");
    assert!(ask(&h, MediaKind::Files, None, 50).items.is_empty());
    assert!(!ask(&h, MediaKind::Visual, None, 50).other);
    // The sticker's picture is in no gallery.
    assert_eq!(ask(&h, MediaKind::Visual, None, 50).items.len(), 1);
}

/// The measurement of the gallery's cost in a big chat (run by hand in a release build:
/// `kcap cargo nextest run --release ... --run-ignored only -E 'test(=chat_media::...)'`): a chat of
/// 50,000 texts with 20 pictures among them; the 6-square section's statement (limit 7) and a
/// page of 48, each asked 200 times; prints the minimum, median and 95th percentile in
/// microseconds (numbers only). The unit test of the plan is what keeps it fast, not this time.
#[test]
#[ignore = "a measurement of a 50,000-message chat; run by hand in a release build"]
fn measure_the_gallery_in_a_chat_of_fifty_thousand_texts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    // One media message first so the chat and the account rows exist, then the texts by SQL.
    ingest(&mut h, vec![media(10, vec![picture(0)])]);
    let c = h.connection();
    let (account, conv): (i64, i64) = c
        .query_row(
            "SELECT account_id, id FROM conversations WHERE conv_key = ?1",
            [key().encode()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("chat");
    c.execute(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 50000)
         INSERT INTO messages(account_id, conversation_id, net_scope, net_author, net_local, origin,
             sort_key, sent_at, received_at, body_kind, body, preview, read_state)
         SELECT ?1, ?2, 0, ?3, 'text' || i, 'Remote', 1000 + i * 1000, 1000 + i * 1000,
             1500 + i * 1000, 'Text', '{\"Text\":{\"body\":\"hello\"}}', 'hello', 'Read'
         FROM n",
        (account, conv, ALICE),
    )
    .expect("fifty thousand texts");
    // Twenty pictures spread over the chat (the newest one among the last thousand texts).
    let mut pictures = Vec::new();
    for k in 0..20_i64 {
        pictures.push(media(
            1_000 + k * 2_400_000 + 7,
            vec![picture(u64::try_from(k).unwrap_or(0) + 1)],
        ));
    }
    ingest(&mut h, pictures);
    let total: i64 = h
        .connection()
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .expect("count");
    assert!(total > 50_000);
    let time = |limit: u32| {
        let mut us: Vec<u128> = (0..200)
            .map(|_| {
                let t = std::time::Instant::now();
                let p = ask(&h, MediaKind::Visual, None, limit);
                std::hint::black_box(&p);
                t.elapsed().as_micros()
            })
            .collect();
        us.sort_unstable();
        (us[0], us[us.len() / 2], us[us.len() * 95 / 100])
    };
    let (min7, med7, p957) = time(7);
    let (min48, med48, p9548) = time(48);
    println!("gallery section (limit 7): min {min7} us, median {med7} us, p95 {p957} us");
    println!("gallery page (limit 48): min {min48} us, median {med48} us, p95 {p9548} us");
    assert!(med7 < 5_000, "the section's statement is under 5 ms");
}

/// The second shape (a photo chat; `0020_chat_media.sql` made it cheap): 20,000 picture messages
/// and five files among them (the oldest of the chat, and four more); the section's statement
/// (Visual 7 and the `EXISTS` for Files), a level page of pictures (48, again with the `EXISTS`),
/// the Files page (48) and a fresh read of 1,000 pictures, each asked 100 times; prints the
/// minimum, median and 95th percentile in microseconds. Run by hand in a release build like the
/// first shape. Before the two indexes of the kinds the `EXISTS` and the Files page walked every
/// picture of the chat: 19 ms (measured by the review of P4, release, this very shape with one
/// file).
#[test]
#[ignore = "a measurement of a chat of 20,000 pictures; run by hand in a release build"]
fn measure_the_gallery_in_a_chat_of_twenty_thousand_pictures_and_some_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    // One picture first so the chat exists, then the pictures by SQL, then the files through
    // the ingest (the oldest of the chat among them).
    ingest(&mut h, vec![media(10, vec![picture(0)])]);
    let c = h.connection();
    let (account, conv): (i64, i64) = c
        .query_row(
            "SELECT account_id, id FROM conversations WHERE conv_key = ?1",
            [key().encode()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("chat");
    c.execute(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20000)
         INSERT INTO messages(account_id, conversation_id, net_scope, net_author, net_local, origin,
             sort_key, sent_at, received_at, body_kind, body, preview, read_state)
         SELECT ?1, ?2, 0, ?3, 'pic' || i, 'Remote', 1000 + i * 1000, 1000 + i * 1000,
             1500 + i * 1000, 'Media', '{}', NULL, 'Read'
         FROM n",
        (account, conv, ALICE),
    )
    .expect("twenty thousand picture messages");
    c.execute(
        "INSERT INTO attachments(message_id, idx, kind, state, mime, size, width, height)
         SELECT id, 0, 'Image', '\"Local\"', 'image/jpeg', 1000, 40, 30 FROM messages
         WHERE net_local LIKE 'pic%'",
        [],
    )
    .expect("their pictures");
    let mut files = Vec::new();
    for (k, at) in [500_i64, 5_000_000, 10_000_000, 15_000_000, 19_000_000]
        .into_iter()
        .enumerate()
    {
        files.push(media(
            at,
            vec![file("f.pdf", u64::try_from(k).unwrap_or(0))],
        ));
    }
    ingest(&mut h, files);
    let flagged: i64 = h
        .connection()
        .query_row(
            "SELECT count(*) FROM messages WHERE media_visual = 1",
            [],
            |r| r.get(0),
        )
        .expect("flags");
    assert!(
        flagged >= 20_000,
        "the triggers flagged the pictures: {flagged}"
    );
    let time = |kind: MediaKind, limit: u32| {
        let mut us: Vec<u128> = (0..100)
            .map(|_| {
                let t = std::time::Instant::now();
                let p = ask(&h, kind, None, limit);
                std::hint::black_box(&p);
                t.elapsed().as_micros()
            })
            .collect();
        us.sort_unstable();
        (us[0], us[us.len() / 2], us[us.len() * 95 / 100])
    };
    let section = time(MediaKind::Visual, 7);
    let level = time(MediaKind::Visual, 48);
    let files = time(MediaKind::Files, 48);
    let fresh = time(MediaKind::Visual, 1_000);
    println!("20,000 pictures, 5 files: section (Visual 7): min/median/p95 {section:?} us");
    println!("20,000 pictures, 5 files: level page (Visual 48): {level:?} us");
    println!("20,000 pictures, 5 files: Files page (48): {files:?} us");
    println!("20,000 pictures, 5 files: fresh read (Visual 1000): {fresh:?} us");
    assert!(section.1 < 5_000, "the section's statement is under 5 ms");
    assert!(level.1 < 5_000, "a level page is under 5 ms");
    assert!(files.1 < 5_000, "the Files page is under 5 ms");
}

/// What the flags' triggers cost the core's ingest of media (`0020_chat_media.sql`: an insert into
/// `attachments` rewrites the flags of its message, and a flagged row adds an entry to a partial
/// index). The 500 messages per second budget test ingests texts only, which fire no trigger, so it
/// cannot see this. Two databases in one process, one with the three triggers and one without
/// (its partial indexes stay empty), take the SAME 100-message batches alternately, 30 rounds each
/// (a load on the machine hits both); prints the median and the minimum of a batch in
/// microseconds, for a message with one picture and for albums of ten. Run by hand in a release
/// build: `kcap cargo nextest run --release -p kourier-core --run-ignored only -E
/// 'test(=chat_media::measure_the_cost_of_the_gallery_triggers_on_media_ingest)' --no-capture`.
/// The numbers are not asserted (timing under load proves nothing); the flags are.
#[test]
#[ignore = "a measurement of media ingest with and without the triggers; run by hand in a release build"]
fn measure_the_cost_of_the_gallery_triggers_on_media_ingest() {
    let dir_with = tempfile::tempdir().expect("tempdir");
    let dir_without = tempfile::tempdir().expect("tempdir");
    let mut with = harness(&dir_with);
    let mut without = harness(&dir_without);
    without
        .connection()
        .execute_batch(
            "DROP TRIGGER attachments_gallery_insert;
             DROP TRIGGER attachments_gallery_delete;
             DROP TRIGGER attachments_gallery_update;",
        )
        .expect("the database without the triggers");
    let batch = |round: i64, album: usize| -> Vec<IncomingMessage> {
        (0..100_i64)
            .map(|i| {
                let at = 1_000_000 + round * 1_000 + i;
                let pictures = (0..album)
                    .map(|j| picture(u64::try_from(at).unwrap_or(0) * 40 + j as u64))
                    .collect();
                media(at, pictures)
            })
            .collect()
    };
    for album in [1_usize, 10] {
        let (mut t_with, mut t_without) = (Vec::new(), Vec::new());
        for round in 0..30_i64 {
            let r = round + if album == 1 { 0 } else { 1_000 };
            let with_first = round % 2 == 0;
            for is_with in [with_first, !with_first] {
                let messages = batch(r, album);
                let t = std::time::Instant::now();
                if is_with {
                    ingest(&mut with, messages);
                    t_with.push(t.elapsed().as_micros());
                } else {
                    ingest(&mut without, messages);
                    t_without.push(t.elapsed().as_micros());
                }
            }
        }
        t_with.sort_unstable();
        t_without.sort_unstable();
        println!(
            "media ingest, {album} picture(s) per message, 100 messages per batch: with the triggers median {} us (min {}), without median {} us (min {})",
            t_with[t_with.len() / 2],
            t_with[0],
            t_without[t_without.len() / 2],
            t_without[0]
        );
    }
    let count = |h: &IngestHarness, sql: &str| -> i64 {
        h.connection()
            .query_row(sql, [], |r| r.get(0))
            .expect("count")
    };
    assert_eq!(
        count(
            &with,
            "SELECT count(*) FROM messages WHERE body_kind = 'Media' AND media_visual = 0"
        ),
        0,
        "the triggers flagged every picture message"
    );
    assert!(
        count(
            &with,
            "SELECT count(*) FROM messages WHERE media_visual = 1"
        ) >= 6_000
    );
    assert_eq!(
        count(
            &without,
            "SELECT count(*) FROM messages WHERE media_visual = 1"
        ),
        0,
        "without the triggers no flag is set (the measurement's baseline)"
    );
}
