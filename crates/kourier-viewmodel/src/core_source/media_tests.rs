//! `CoreSource` and the gallery slots (people pages P4): the section's six newest, the pages of
//! the level from the cursor, the refresh that keeps what was loaded, a deleted picture leaving,
//! what an answer that changed nothing tells, the chat that goes away or is merged; against the
//! simulated core of [`super::tests`].

use kourier_core::facade::{ChatMediaItem, ChatMediaQuery, MediaCursor, MediaKind, Query};
use kourier_model::{
    AccountId, Attachment, AttachmentId, AttachmentKind, AttachmentState, ConversationId, MessageId,
};

use super::tests::{Sent, Store, result, source, take_queries};
use super::*;
use crate::chat_media::{
    MediaRow, MediaSlot, MediaUpdate, PAGE, SECTION_ASK, SECTION_TILES, VIEWER_ASK,
};
use crate::timeline::MediaState;

const CONV: ConversationId = ConversationId(5);
const ACCOUNT: AccountId = AccountId(1);

fn item(message: i64, idx: u32, kind: AttachmentKind) -> ChatMediaItem {
    ChatMediaItem {
        message: MessageId(message),
        sort_key: message * 10,
        idx,
        attachment: Attachment {
            id: Some(AttachmentId(message * 100 + i64::from(idx))),
            kind,
            mime: match kind {
                AttachmentKind::File => "application/pdf".into(),
                _ => "image/jpeg".into(),
            },
            size: 2_048,
            file_name: (kind == AttachmentKind::File).then(|| "plan.pdf".to_owned()),
            dims: Some((40, 30)),
            state: AttachmentState::Local,
            ..Attachment::default()
        },
        save: true,
    }
}

/// `n` pictures, one per message, the newest message first (as the core lists them).
fn pictures(n: i64) -> Vec<ChatMediaItem> {
    (1..=n)
        .rev()
        .map(|m| item(m, 0, AttachmentKind::Image))
        .collect()
}

/// A source with the account and the list read, as the page tests of P1 and P2 have it.
fn opened(store: &Store) -> (CoreSource, Sent) {
    let (mut src, sent) = source();
    src.reload();
    serve(&mut src, &sent, store);
    (src, sent)
}

/// Answers every query from `store`, in order, and returns what the gallery was asked.
fn serve(src: &mut CoreSource, sent: &Sent, store: &Store) -> (Changes, Vec<ChatMediaQuery>) {
    let mut ch = Changes::default();
    let mut asked = Vec::new();
    for _ in 0..100 {
        let qs = take_queries(sent);
        if qs.is_empty() {
            return (ch, asked);
        }
        for (t, q) in qs {
            if let Query::ChatMedia(m) = &q {
                asked.push(m.clone());
            }
            ch.merge(src.on_event(&result(t, store.answer(&q))));
        }
    }
    panic!("queries do not settle");
}

/// The gallery questions waiting in `sent` (the others are taken out and dropped).
fn asked_media(sent: &Sent) -> Vec<ChatMediaQuery> {
    take_queries(sent)
        .into_iter()
        .filter_map(|(_, q)| match q {
            Query::ChatMedia(m) => Some(m),
            _ => None,
        })
        .collect()
}

fn update_of(src: &mut CoreSource, slot: MediaSlot) -> Option<MediaUpdate> {
    src.take_chat_media().into_iter().find(|u| u.slot == slot)
}

fn messages(u: &MediaUpdate) -> Vec<i64> {
    u.rows.iter().map(|r| r.message.0).collect()
}

/// A core with a gallery and the chats the tests open it for (5, 6 and 9) in its list: a gallery
/// is shown outside the open chat only for a chat the list holds and does not call protected.
fn with_media(media: Vec<ChatMediaItem>) -> Store {
    let mut store = Store::new(&[(1, 3), (5, 0), (6, 0), (9, 0)]);
    store.media = media;
    store
}

/// A section shows the six newest pictures and says "more" when one was left out; with exactly
/// six it does not; a chat with no picture but a file still says the other kind has something.
#[test]
fn a_section_shows_the_six_newest_and_says_when_there_are_more() {
    let store = with_media(pictures(9));
    let (mut src, sent) = opened(&store);
    assert!(src.open_chat_media(MediaSlot::PersonSection, ACCOUNT, CONV));
    let (ch, asked) = serve(&mut src, &sent, &store);
    assert!(ch.chat_media);
    assert_eq!(asked.len(), 1);
    assert_eq!(
        (
            asked[0].conv,
            asked[0].kind,
            asked[0].before,
            asked[0].limit
        ),
        (CONV, MediaKind::Visual, None, 7),
        "one more than the tiles: that one says there is more"
    );
    let u = update_of(&mut src, MediaSlot::PersonSection).expect("an update");
    assert_eq!(
        messages(&u),
        [9, 8, 7, 6, 5, 4],
        "the six newest, newest first"
    );
    assert!(u.more && !u.other);
    assert!(src.take_chat_media().is_empty(), "taken once");

    let store = with_media(pictures(6));
    let (mut src, sent) = opened(&store);
    src.open_chat_media(MediaSlot::GroupSection, ACCOUNT, CONV);
    serve(&mut src, &sent, &store);
    let u = update_of(&mut src, MediaSlot::GroupSection).expect("an update");
    assert_eq!(u.rows.len(), SECTION_TILES);
    assert!(!u.more, "six fit: nothing is left out");

    let mut store = with_media(vec![item(3, 0, AttachmentKind::File)]);
    let (mut src, sent) = opened(&store);
    src.open_chat_media(MediaSlot::PersonSection, ACCOUNT, CONV);
    serve(&mut src, &sent, &store);
    let u = update_of(&mut src, MediaSlot::PersonSection).expect("an update");
    assert!(
        u.rows.is_empty() && !u.more && u.other,
        "no picture, but files"
    );
    store.media.clear();
}

/// S-P: a protected chat's gallery shows only inside the open chat: its person's page opened
/// from elsewhere (a group's member) asks the core nothing and shows no section; opened inside
/// the chat itself it shows the pictures as any other.
#[test]
fn a_protected_chat_s_gallery_shows_only_inside_the_open_chat() {
    let mut store = with_media(pictures(3));
    // (The gallery's chat is listed, and protected.)
    store.protected = vec![CONV.0];
    let (mut src, sent) = opened(&store);
    for slot in [MediaSlot::PersonSection, MediaSlot::Media, MediaSlot::Files] {
        assert!(src.open_chat_media(slot, ACCOUNT, CONV));
        let (_, asked) = serve(&mut src, &sent, &store);
        assert!(asked.is_empty(), "{slot:?}: nothing asked");
        let u = update_of(&mut src, slot).expect("an update");
        assert!(
            u.rows.is_empty() && !u.more && !u.other,
            "{slot:?}: nothing shown"
        );
    }
    src.open_conversation(Some(CONV));
    serve(&mut src, &sent, &store);
    assert!(src.open_chat_media(MediaSlot::PersonSection, ACCOUNT, CONV));
    let (_, asked) = serve(&mut src, &sent, &store);
    assert_eq!(asked.len(), 1, "inside the open chat it is read");
    let u = update_of(&mut src, MediaSlot::PersonSection).expect("an update");
    assert_eq!(messages(&u), [3, 2, 1]);

    // The same chat, not protected: read from anywhere.
    let store = with_media(pictures(3));
    let (mut src, sent) = opened(&store);
    src.open_chat_media(MediaSlot::PersonSection, ACCOUNT, CONV);
    let (_, asked) = serve(&mut src, &sent, &store);
    assert_eq!(asked.len(), 1);
}

/// S-P review L1: the gallery rule FAILS CLOSED and follows the list. Before the first list read,
/// and for a chat the list does not hold, nothing is asked and nothing shown; a list read that
/// lists the chat shows it, one that flips its `protected` on takes the pictures and file names
/// away at once (a person page's section stays open), one that lifts it brings them back, and
/// one that no longer holds the chat takes them away again. The slots run again by themselves.
#[test]
fn a_gallery_follows_the_list_and_fails_closed() {
    let mut store = with_media(pictures(3));
    let (mut src, sent) = source();
    // The list is not read yet: the section is opened and shows nothing, asks nothing.
    assert!(src.open_chat_media(MediaSlot::PersonSection, ACCOUNT, CONV));
    assert!(
        asked_media(&sent).is_empty(),
        "nothing asked before the list"
    );
    let nothing = |u: &MediaUpdate| u.rows.is_empty() && !u.more && !u.other;
    assert!(nothing(
        &update_of(&mut src, MediaSlot::PersonSection).expect("told")
    ));
    // The first read of the list holds the chat, not protected: the slot reads by itself.
    src.reload();
    let (ch, asked) = serve(&mut src, &sent, &store);
    assert!(ch.chat_media);
    assert_eq!(asked.len(), 1, "the list read brought the gallery");
    let u = update_of(&mut src, MediaSlot::PersonSection).expect("an update");
    assert_eq!(messages(&u), [3, 2, 1]);

    // The phone protects the chat: the next list read takes the pictures away, asks nothing.
    store.protected = vec![CONV.0];
    src.on_event(&UiEvent::ConversationsChanged { ids: vec![CONV] });
    let (ch, asked) = serve(&mut src, &sent, &store);
    assert!(ch.chat_media && asked.is_empty(), "nothing asked");
    let u = update_of(&mut src, MediaSlot::PersonSection).expect("told to show nothing");
    assert!(
        nothing(&u),
        "the section is empty once the chat is protected"
    );

    // Lifted again: the pictures come back, by themselves.
    store.protected.clear();
    src.on_event(&UiEvent::ConversationsChanged { ids: vec![CONV] });
    let (ch, asked) = serve(&mut src, &sent, &store);
    assert!(ch.chat_media);
    assert_eq!(asked.len(), 1);
    assert_eq!(
        messages(&update_of(&mut src, MediaSlot::PersonSection).expect("an update")),
        [3, 2, 1]
    );

    // The chat is no longer in the list (hidden): fail closed, nothing shown; listed again, shown.
    let listed = store.convs.remove(&CONV.0).expect("listed");
    src.on_event(&UiEvent::ConversationsChanged { ids: vec![CONV] });
    let (_, asked) = serve(&mut src, &sent, &store);
    assert!(asked.is_empty());
    assert!(nothing(
        &update_of(&mut src, MediaSlot::PersonSection).expect("told")
    ));
    store.convs.insert(CONV.0, listed);
    src.on_event(&UiEvent::ConversationsChanged { ids: vec![CONV] });
    let (_, asked) = serve(&mut src, &sent, &store);
    assert_eq!(asked.len(), 1);
    assert_eq!(
        messages(&update_of(&mut src, MediaSlot::PersonSection).expect("an update")),
        [3, 2, 1]
    );

    // The flag flips while the gallery is being read: the answer on its way shows nothing.
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: Vec::new(),
    });
    let in_flight = take_queries(&sent);
    assert_eq!(in_flight.len(), 1, "the gallery is read again");
    // (A picture arrived: the answer on its way holds ANOTHER list than the slot handed over,
    // so it would be handed over as an update if the withheld slot did not drop it.)
    store.media.insert(0, item(4, 0, AttachmentKind::Image));
    store.protected = vec![CONV.0];
    src.on_event(&UiEvent::ConversationsChanged { ids: vec![CONV] });
    for (t, q) in take_queries(&sent) {
        // (The list is read and answered first: the chat is protected now.)
        assert!(matches!(q, Query::Conversations(_)));
        src.on_event(&result(t, store.answer(&q)));
    }
    for (t, q) in in_flight {
        // (Then the answer of the gallery read that was already out: pictures.)
        src.on_event(&result(t, store.answer(&q)));
    }
    let (_, asked) = serve(&mut src, &sent, &store);
    assert!(asked.is_empty(), "the withheld gallery is not read again");
    // The slot's FIRST update after the flag flipped is the empty one, and it is the only one:
    // the answer that was already out (pictures [4, 3, 2, 1]) shows nothing.
    let updates = src.take_chat_media();
    assert_eq!(updates.len(), 1, "one update, the empty one: {updates:?}");
    assert_eq!(updates[0].slot, MediaSlot::PersonSection);
    assert!(
        nothing(&updates[0]),
        "no picture of a chat that turned protected"
    );
}

/// The level pages by the cursor of the last row it has: the next page is appended, the order
/// is kept, no attachment is twice, and "more" is true until the last page.
#[test]
fn the_level_pages_from_the_cursor_and_appends() {
    let store = with_media(pictures(100));
    let (mut src, sent) = opened(&store);
    assert!(src.open_chat_media(MediaSlot::Media, ACCOUNT, CONV));
    let (_, asked) = serve(&mut src, &sent, &store);
    assert_eq!((asked[0].before, asked[0].limit), (None, PAGE));
    let u = update_of(&mut src, MediaSlot::Media).expect("the first page");
    assert_eq!(u.rows.len(), PAGE as usize);
    assert!(u.more);

    src.more_chat_media(MediaSlot::Media);
    let (ch, asked) = serve(&mut src, &sent, &store);
    assert!(ch.chat_media);
    assert_eq!(
        asked[0].before,
        Some(MediaCursor {
            sort_key: 530,
            message: MessageId(53),
            idx: Some(0)
        }),
        "after the last row of the first page"
    );
    let u = update_of(&mut src, MediaSlot::Media).expect("the second page");
    assert_eq!(u.rows.len(), 2 * PAGE as usize);
    assert!(u.more);
    src.more_chat_media(MediaSlot::Media);
    serve(&mut src, &sent, &store);
    let u = update_of(&mut src, MediaSlot::Media).expect("the last page");
    assert_eq!(u.rows.len(), 100);
    assert!(!u.more, "the last page says so");
    let all: Vec<i64> = (1..=100).rev().collect();
    assert_eq!(messages(&u), all, "every picture once, newest first");
    // Nothing more to ask once the last page is here.
    src.more_chat_media(MediaSlot::Media);
    assert!(asked_media(&sent).is_empty());
    // A section never goes on.
    src.open_chat_media(MediaSlot::GroupSection, ACCOUNT, CONV);
    serve(&mut src, &sent, &store);
    src.more_chat_media(MediaSlot::GroupSection);
    assert!(asked_media(&sent).is_empty());
}

/// A change of the chat reads the rows again from the newest, as many as are loaded: a picture
/// that was deleted is gone from the section and from the level, a new one comes first, and the
/// second page the reader loaded is still there.
#[test]
fn a_change_reads_again_and_a_deleted_picture_leaves() {
    let mut store = with_media(pictures(60));
    let (mut src, sent) = opened(&store);
    src.open_chat_media(MediaSlot::PersonSection, ACCOUNT, CONV);
    src.open_chat_media(MediaSlot::Media, ACCOUNT, CONV);
    serve(&mut src, &sent, &store);
    src.more_chat_media(MediaSlot::Media);
    serve(&mut src, &sent, &store);
    let shown = src.take_chat_media();
    let level = shown
        .iter()
        .find(|u| u.slot == MediaSlot::Media)
        .expect("level");
    assert_eq!(level.rows.len(), 60);

    // The newest picture is deleted.
    store.media.retain(|i| i.message.0 != 60);
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: vec![MessageId(60)],
    });
    let (ch, asked) = serve(&mut src, &sent, &store);
    assert!(ch.chat_media);
    let level_ask = asked
        .iter()
        .find(|q| q.limit >= PAGE)
        .expect("the level's read");
    assert_eq!(
        (level_ask.before, level_ask.limit),
        (None, 60),
        "from the newest, as many as were loaded"
    );
    let updates = src.take_chat_media();
    let section = updates
        .iter()
        .find(|u| u.slot == MediaSlot::PersonSection)
        .expect("section");
    assert_eq!(
        messages(section),
        [59, 58, 57, 56, 55, 54],
        "the deleted one left the section"
    );
    let level = updates
        .iter()
        .find(|u| u.slot == MediaSlot::Media)
        .expect("level");
    assert_eq!(level.rows.len(), 59);
    assert!(!level.rows.iter().any(|r| r.message.0 == 60));
    assert!(!level.more);

    // A new picture comes first.
    store.media.insert(0, item(61, 0, AttachmentKind::Image));
    src.on_event(&UiEvent::TimelineInvalidated { conv: CONV });
    serve(&mut src, &sent, &store);
    let updates = src.take_chat_media();
    let section = updates
        .iter()
        .find(|u| u.slot == MediaSlot::PersonSection)
        .expect("section");
    assert_eq!(messages(section)[0], 61);
}

/// A fresh read replaces everything loaded: past what one query reads (the simulated core answers
/// 48 at most) the rows after its window go too, so a picture deleted there never stays; the
/// level's "more" then loads them again from the cursor of the last row it has.
#[test]
fn a_fresh_read_cuts_what_lies_after_its_window_and_more_loads_it_again() {
    let mut store = with_media(pictures(100));
    let (mut src, sent) = opened(&store);
    src.open_chat_media(MediaSlot::Media, ACCOUNT, CONV);
    serve(&mut src, &sent, &store);
    src.more_chat_media(MediaSlot::Media);
    serve(&mut src, &sent, &store);
    assert_eq!(src.take_chat_media()[0].rows.len(), 96);

    // From now on the core answers 48 at most: the read of the 96 loaded rows gets the first 48.
    // A picture far down the list (message 5, row 96) is deleted meanwhile.
    store.media_cap = Some(PAGE);
    store.media.retain(|i| i.message.0 != 5);
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: Vec::new(),
    });
    let (ch, asked) = serve(&mut src, &sent, &store);
    assert_eq!(asked[0].limit, 96);
    assert!(ch.chat_media);
    let u = update_of(&mut src, MediaSlot::Media).expect("an update");
    assert_eq!(u.rows.len(), 48, "only what the read got: the rest is cut");
    assert_eq!(messages(&u)[0], 100);
    assert!(u.more, "and more follow");

    // "More" loads the rest again from the last row: the deleted picture is not in it.
    src.more_chat_media(MediaSlot::Media);
    serve(&mut src, &sent, &store);
    let u = update_of(&mut src, MediaSlot::Media).expect("the next page");
    assert_eq!(u.rows.len(), 96);
    assert!(!u.rows.iter().any(|r| r.message.0 == 5));
    assert!(!u.more || u.rows.len() == 96);
}

/// A change of the chat and a "more" that arrive while one query is in flight are BOTH served: the
/// answer of the fresh read is followed by the page after its last row (before this was kept, the
/// tracker had one follow-up for two wishes, the fresh read took it and the page was lost for
/// good: the level stopped at 48 rows however far it was scrolled).
#[test]
fn a_change_and_a_more_during_one_query_are_both_served() {
    let store = with_media(pictures(100));
    let (mut src, sent) = opened(&store);
    src.open_chat_media(MediaSlot::Media, ACCOUNT, CONV);
    serve(&mut src, &sent, &store);
    assert_eq!(
        update_of(&mut src, MediaSlot::Media)
            .expect("rows")
            .rows
            .len(),
        48
    );

    // A change: a fresh read is in flight. A second change and the "more" come meanwhile.
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: Vec::new(),
    });
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: Vec::new(),
    });
    src.more_chat_media(MediaSlot::Media);
    let (_, asked) = serve(&mut src, &sent, &store);
    assert!(
        asked.iter().any(|q| q.before.is_some()),
        "the page after the last row was asked: {asked:?}"
    );
    let u = update_of(&mut src, MediaSlot::Media).expect("an update");
    assert_eq!(
        u.rows.len(),
        96,
        "both the new read and the next page arrived"
    );

    // The same during a burst: each change comes with the "more" and every one is served in the end.
    for _ in 0..3 {
        src.on_event(&UiEvent::MessagesChanged {
            conv: CONV,
            ids: Vec::new(),
        });
        src.more_chat_media(MediaSlot::Media);
        serve(&mut src, &sent, &store);
    }
    assert_eq!(
        update_of(&mut src, MediaSlot::Media).map(|u| u.rows.len()),
        Some(100)
    );
}

/// An answer that equals what the front end has tells nothing: a message in a busy chat asks
/// the gallery again, and nearly every answer is the same.
#[test]
fn an_answer_that_changed_nothing_tells_nothing() {
    let store = with_media(pictures(8));
    let (mut src, sent) = opened(&store);
    src.open_chat_media(MediaSlot::PersonSection, ACCOUNT, CONV);
    serve(&mut src, &sent, &store);
    assert!(update_of(&mut src, MediaSlot::PersonSection).is_some());
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: vec![MessageId(1)],
    });
    let (ch, asked) = serve(&mut src, &sent, &store);
    assert_eq!(asked.len(), 1, "it is read again");
    assert!(!ch.chat_media, "and the same answer changes nothing");
    assert!(src.take_chat_media().is_empty());
    // A message of another chat does not even ask.
    src.on_event(&UiEvent::MessagesChanged {
        conv: ConversationId(77),
        ids: vec![MessageId(1)],
    });
    let (_, asked) = serve(&mut src, &sent, &store);
    assert!(asked.is_empty());
}

/// Several changes in a row are one question in flight and one follow-up; closing a slot ends
/// its questions (an answer still on its way is dropped), and a closed slot asks nothing.
#[test]
fn changes_coalesce_and_a_closed_slot_asks_nothing() {
    let store = with_media(pictures(8));
    let (mut src, sent) = opened(&store);
    src.open_chat_media(MediaSlot::GroupSection, ACCOUNT, CONV);
    for _ in 0..4 {
        src.on_event(&UiEvent::MessagesChanged {
            conv: CONV,
            ids: Vec::new(),
        });
    }
    let (_, asked) = serve(&mut src, &sent, &store);
    assert_eq!(
        asked.len(),
        2,
        "the first question and one follow-up for the rest"
    );

    // A question in flight when the slot closes: its answer is not shown.
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: Vec::new(),
    });
    let in_flight = take_queries(&sent);
    assert_eq!(in_flight.len(), 1);
    src.close_chat_media(MediaSlot::GroupSection);
    for (t, q) in in_flight {
        let ch = src.on_event(&result(t, store.answer(&q)));
        assert!(!ch.chat_media);
    }
    assert!(src.take_chat_media().is_empty());
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: Vec::new(),
    });
    assert!(asked_media(&sent).is_empty(), "a closed slot asks nothing");
}

/// A chat that is removed closes the slots that showed it, and they are told to show nothing; a
/// chat merged into another is shown from the survivor; the slots of two chats do not mix.
#[test]
fn a_removed_chat_closes_its_slots_a_merged_one_follows_and_two_chats_do_not_mix() {
    let mut store = with_media(pictures(3));
    let (mut src, sent) = opened(&store);
    src.open_chat_media(MediaSlot::GroupSection, ACCOUNT, CONV);
    src.open_chat_media(MediaSlot::PersonSection, ACCOUNT, ConversationId(6));
    serve(&mut src, &sent, &store);
    src.take_chat_media();
    // A change of one chat reads that chat's slot only.
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: Vec::new(),
    });
    let (_, asked) = serve(&mut src, &sent, &store);
    assert_eq!(asked.iter().map(|q| q.conv).collect::<Vec<_>>(), [CONV]);

    // Chat 5 is merged into chat 9: its slot shows chat 9.
    store.media.truncate(2);
    src.on_event(&UiEvent::ConversationMerged {
        from: CONV,
        into: ConversationId(9),
    });
    let (_, asked) = serve(&mut src, &sent, &store);
    assert_eq!(
        asked.iter().map(|q| q.conv).collect::<Vec<_>>(),
        [ConversationId(9)]
    );
    assert_eq!(
        update_of(&mut src, MediaSlot::GroupSection).map(|u| u.rows.len()),
        Some(2)
    );

    // Chat 9 is removed: its slot closes and says nothing is left; the other chat's does not.
    let ch = src.on_event(&UiEvent::ConversationsRemoved {
        ids: vec![ConversationId(9)],
    });
    assert!(ch.chat_media);
    let ended = update_of(&mut src, MediaSlot::GroupSection).expect("told");
    assert!(ended.rows.is_empty() && !ended.more && !ended.other);
    src.on_event(&UiEvent::MessagesChanged {
        conv: ConversationId(9),
        ids: Vec::new(),
    });
    assert!(
        asked_media(&sent).is_empty(),
        "the closed slot asks nothing"
    );
    src.on_event(&UiEvent::MessagesChanged {
        conv: ConversationId(6),
        ids: Vec::new(),
    });
    assert_eq!(
        serve(&mut src, &sent, &store).1.len(),
        1,
        "the other chat's slot lives"
    );
}

/// The row of an attachment is the timeline's own block made flat: a picture that is here has
/// its thumbnail and its viewer URL, one that is not has only its blurhash, a video is a clip
/// (no still), a GIF plays from its copy, and a file has its name, detail and icon.
#[test]
fn a_row_is_the_timelines_block_made_flat() {
    let row = |mut i: ChatMediaItem, f: &dyn Fn(&mut Attachment), kind| {
        f(&mut i.attachment);
        MediaRow::new(&i, kind).expect("a row")
    };
    let here = row(
        item(1, 0, AttachmentKind::Image),
        &|_| {},
        MediaKind::Visual,
    );
    assert!(here.url.starts_with("image://kourier/thumb/100?v="));
    assert!(here.full_url.starts_with("image://kourier/att/100?v="));
    assert_eq!(
        (here.state, here.kind, here.width, here.height),
        (MediaState::Local, AttachmentKind::Image, 40, 30)
    );
    assert!(here.media && !here.animated);

    let remote = row(
        item(2, 0, AttachmentKind::Image),
        &|a| {
            a.state = AttachmentState::Remote;
            a.blurhash = Some("LEHV6nWB2yk8pyo0adR*.7kCMdnj".into());
        },
        MediaKind::Visual,
    );
    assert!(remote.url.is_empty() && remote.full_url.is_empty());
    assert!(
        remote
            .placeholder_url
            .starts_with("image://kourier/blurhash/200?v=")
    );
    assert_eq!(remote.state, MediaState::Remote);

    let clip = row(
        item(3, 0, AttachmentKind::Video),
        &|a| {
            a.mime = "video/mp4".into();
            a.duration_ms = Some(42_000);
        },
        MediaKind::Visual,
    );
    assert!(
        clip.url.is_empty() && clip.full_url.is_empty(),
        "a clip has no still"
    );
    assert_eq!(
        (clip.kind, clip.label.as_str()),
        (AttachmentKind::Video, "0:42")
    );

    let gif = row(
        item(4, 0, AttachmentKind::Gif),
        &|a| a.mime = "image/gif".into(),
        MediaKind::Visual,
    );
    assert!(
        gif.animated && gif.full_url.is_empty(),
        "plays from its copy"
    );

    let file = row(item(5, 0, AttachmentKind::File), &|_| {}, MediaKind::Files);
    assert_eq!(file.name, "plan.pdf");
    assert_eq!(file.detail, "2.0 KB · PDF");
    assert_eq!(file.icon, "application-pdf");
    assert!(file.url.is_empty());

    // An item of the other kind is no row of this one, and one without an id is none.
    assert!(MediaRow::new(&item(6, 0, AttachmentKind::File), MediaKind::Visual).is_none());
    let mut anon = item(7, 0, AttachmentKind::Image);
    anon.attachment.id = None;
    assert!(MediaRow::new(&anon, MediaKind::Visual).is_none());
    // The cursor of a row names its message and place.
    assert_eq!(
        here.cursor(),
        MediaCursor {
            sort_key: 10,
            message: MessageId(1),
            idx: Some(0)
        }
    );
}

/// What changed between two versions of one attachment's row names the roles, so a list model
/// announces `dataChanged` for those alone.
#[test]
fn a_download_changes_the_state_and_the_urls_of_a_row_and_nothing_else() {
    use crate::chat_media::media_role::*;
    let mut a = item(1, 0, AttachmentKind::Image);
    a.attachment.state = AttachmentState::Remote;
    let a = MediaRow::new(&a, MediaKind::Visual).expect("row");
    let b = MediaRow::new(&item(1, 0, AttachmentKind::Image), MediaKind::Visual).expect("row");
    let roles = MediaRow::changed_roles(&a, &b);
    for r in [STATE, URL, FULL_URL, PROGRESS] {
        assert!(roles.contains(r), "role {r}");
    }
    for r in [ATTACHMENT, MESSAGE, KIND, WIDTH, HEIGHT, NAME, DETAIL, ICON] {
        assert!(!roles.contains(r), "role {r}");
    }
    assert!(MediaRow::changed_roles(&b, &b).is_empty());
}

/// The slots end with their account, as the person's and the group's pages do (one origin rule):
/// the slots of its chats are closed and told to show nothing; the slots of another account's
/// chats are left alone.
#[test]
fn a_removed_account_ends_the_slots_of_its_chats() {
    let store = with_media(pictures(3));
    let (mut src, sent) = opened(&store);
    src.open_chat_media(MediaSlot::GroupSection, ACCOUNT, CONV);
    src.open_chat_media(MediaSlot::PersonSection, AccountId(2), ConversationId(6));
    serve(&mut src, &sent, &store);
    src.take_chat_media();

    let ch = src.on_event(&UiEvent::AccountRemoved { account: ACCOUNT });
    assert!(ch.chat_media);
    let updates = src.take_chat_media();
    assert_eq!(updates.len(), 1, "only the removed account's slot is told");
    assert_eq!(updates[0].slot, MediaSlot::GroupSection);
    assert!(updates[0].rows.is_empty() && !updates[0].more && !updates[0].other);
    // It asks nothing any more; the other account's slot lives on.
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: Vec::new(),
    });
    assert!(asked_media(&sent).is_empty());
    src.on_event(&UiEvent::MessagesChanged {
        conv: ConversationId(6),
        ids: Vec::new(),
    });
    assert_eq!(serve(&mut src, &sent, &store).1.len(), 1);
}

/// A row that is downloading shows the live progress (`Progress::Download`) when the shown
/// percentage moves, and nothing is handed over for a progress that moves less than a percent,
/// for an attachment no slot holds or for a row that is not downloading; once the state is read
/// again as "here" the live number is dropped (it would otherwise lie over the next download).
#[test]
fn a_downloading_row_shows_the_live_progress() {
    let mut store = with_media(pictures(3));
    // Message 3 is downloading (0 of 100 bytes), the others are here.
    store.media[0].attachment.state = AttachmentState::Downloading {
        done: 0,
        total: 100,
    };
    let down = store.media[0].attachment.id.expect("id");
    let (mut src, sent) = opened(&store);
    src.open_chat_media(MediaSlot::PersonSection, ACCOUNT, CONV);
    serve(&mut src, &sent, &store);
    let first = update_of(&mut src, MediaSlot::PersonSection).expect("rows");
    assert_eq!(first.rows[0].state, MediaState::Downloading);
    assert_eq!(first.rows[0].progress, 0.0);

    let progress = |att: AttachmentId, done: u64, total: u64| {
        UiEvent::Progress(kourier_core::facade::Progress::Download {
            attachment: att,
            done,
            total,
        })
    };
    let ch = src.on_event(&progress(down, 40, 100));
    assert!(ch.chat_media);
    let u = update_of(&mut src, MediaSlot::PersonSection).expect("the live progress");
    assert!(
        (u.rows[0].progress - 0.4).abs() < 1e-6,
        "{}",
        u.rows[0].progress
    );
    assert_eq!(u.rows[1].progress, 1.0, "a row that is here is untouched");
    // The same percentage, and less than a percent further: nothing to tell.
    assert!(!src.on_event(&progress(down, 40, 100)).chat_media);
    assert!(!src.on_event(&progress(down, 405, 1000)).chat_media);
    // Another attachment, and an empty total: nothing either.
    assert!(
        !src.on_event(&progress(AttachmentId(999_999), 10, 100))
            .chat_media
    );
    assert!(!src.on_event(&progress(down, 10, 0)).chat_media);
    // The next percent moves it again.
    assert!(src.on_event(&progress(down, 41, 100)).chat_media);
    src.take_chat_media();

    // The file is here: the row is read as local and the live number goes with the state.
    store.media[0].attachment.state = AttachmentState::Local;
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: Vec::new(),
    });
    serve(&mut src, &sent, &store);
    let u = update_of(&mut src, MediaSlot::PersonSection).expect("local now");
    assert_eq!(u.rows[0].state, MediaState::Local);
    assert_eq!(u.rows[0].progress, 1.0);
    // A late progress event of that download changes nothing (the row is not downloading).
    assert!(!src.on_event(&progress(down, 90, 100)).chat_media);
    // A new download of the same file (a retry) starts from nothing: the old live number is gone.
    store.media[0].attachment.state = AttachmentState::Downloading {
        done: 0,
        total: 100,
    };
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: Vec::new(),
    });
    serve(&mut src, &sent, &store);
    let u = update_of(&mut src, MediaSlot::PersonSection).expect("downloading again");
    assert_eq!(u.rows[0].state, MediaState::Downloading);
    assert_eq!(u.rows[0].progress, 0.0, "the earlier download's number");
}

/// A viewer opened from a section's square reads the WHOLE album of that message with a query of
/// its own, AT the message (the section holds six squares and cuts the album), and follows the
/// message: a picture that arrives in the chat changes the section's squares and not the viewer's
/// album (the viewer stays), and the viewer is told to show nothing only when the message has no
/// picture left. A message that is not among the rows it is opened from opens nothing.
#[test]
fn a_viewer_reads_the_whole_album_at_its_message_and_follows_the_message() {
    // Newest first: 12, 11, 10, 9, then message 8 with three pictures, then 7.
    let mut media: Vec<ChatMediaItem> = (9..=12)
        .rev()
        .map(|m| item(m, 0, AttachmentKind::Image))
        .collect();
    media.extend((0..3).map(|i| item(8, i, AttachmentKind::Image)));
    media.push(item(7, 0, AttachmentKind::Image));
    let mut store = with_media(media);
    let (mut src, sent) = opened(&store);
    src.open_chat_media(MediaSlot::PersonSection, ACCOUNT, CONV);
    serve(&mut src, &sent, &store);
    let section = update_of(&mut src, MediaSlot::PersonSection).expect("the section");
    assert_eq!(
        messages(&section),
        [12, 11, 10, 9, 8, 8],
        "the album is cut at six squares"
    );

    assert!(!src.open_media_viewer(MediaSlot::PersonSection, MessageId(99)));
    assert!(!src.open_media_viewer(MediaSlot::Viewer, MessageId(8)));
    assert!(
        !src.open_media_viewer(MediaSlot::Files, MessageId(8)),
        "a slot that holds nothing"
    );
    assert!(src.open_media_viewer(MediaSlot::PersonSection, MessageId(8)));
    let (ch, asked) = serve(&mut src, &sent, &store);
    assert!(ch.chat_media);
    assert_eq!(
        asked,
        [ChatMediaQuery {
            conv: CONV,
            kind: MediaKind::Visual,
            before: Some(MediaCursor {
                sort_key: 80,
                message: MessageId(8),
                idx: None
            }),
            limit: VIEWER_ASK,
        }],
        "AT the message (its first picture included), a few more than an album"
    );
    let album = update_of(&mut src, MediaSlot::Viewer).expect("the viewer's album");
    assert_eq!(
        messages(&album),
        [8, 8, 8],
        "the whole album and nothing of the older message"
    );
    assert_eq!(
        album
            .rows
            .iter()
            .map(|r| r.attachment.0)
            .collect::<Vec<_>>(),
        [800, 801, 802]
    );
    assert!(!album.more && !album.other);

    // A picture arrives in the chat: the section's squares move on, the viewer's album does not
    // (nothing is handed over for it: an equal answer tells nothing), and it asked again.
    store.media.insert(0, item(13, 0, AttachmentKind::Image));
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: Vec::new(),
    });
    let (_, asked) = serve(&mut src, &sent, &store);
    assert!(
        asked
            .iter()
            .any(|q| q.before.is_some_and(|c| c.idx.is_none())),
        "the viewer's slot reads again with the chat"
    );
    let updates = src.take_chat_media();
    assert!(
        updates.iter().any(|u| u.slot == MediaSlot::PersonSection),
        "the section moved on"
    );
    assert!(
        updates.iter().all(|u| u.slot != MediaSlot::Viewer),
        "the viewer's album is the same: nothing is told"
    );

    // The message loses its pictures (deleted, expired, swept): the viewer is told to show nothing.
    store.media.retain(|i| i.message.0 != 8);
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: Vec::new(),
    });
    serve(&mut src, &sent, &store);
    let gone = update_of(&mut src, MediaSlot::Viewer).expect("the viewer is told");
    assert!(gone.rows.is_empty());

    // Closed, the slot asks nothing any more.
    src.close_chat_media(MediaSlot::Viewer);
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: Vec::new(),
    });
    let (_, asked) = serve(&mut src, &sent, &store);
    assert_eq!(
        asked.len(),
        1,
        "only the section (the one slot still open) asks: {asked:?}"
    );
    assert!(asked[0].before.is_none() && asked[0].limit == SECTION_ASK);
}

/// The level's grid hands the viewer the same way: the viewer's album does not depend on the rows
/// the grid holds (a fresh read keeps at most `MAX_MEDIA_PAGE` of them).
#[test]
fn a_viewer_opened_from_the_level_reads_its_own_album() {
    let mut media: Vec<ChatMediaItem> = (3..=60)
        .rev()
        .map(|m| item(m, 0, AttachmentKind::Image))
        .collect();
    media.extend((0..3).map(|i| item(2, i, AttachmentKind::Image)));
    let store = with_media(media);
    let (mut src, sent) = opened(&store);
    src.open_chat_media(MediaSlot::Media, ACCOUNT, CONV);
    serve(&mut src, &sent, &store);
    assert_eq!(
        update_of(&mut src, MediaSlot::Media)
            .expect("the grid")
            .rows
            .len(),
        PAGE as usize
    );
    // Message 2 is not among the grid's first page: the viewer cannot be opened from there ...
    assert!(!src.open_media_viewer(MediaSlot::Media, MessageId(2)));
    // ... one that is, reads its own rows whatever the grid does.
    assert!(src.open_media_viewer(MediaSlot::Media, MessageId(60)));
    serve(&mut src, &sent, &store);
    let album = update_of(&mut src, MediaSlot::Viewer).expect("the album");
    assert_eq!(messages(&album), [60]);
    src.close_chat_media(MediaSlot::Media);
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: Vec::new(),
    });
    serve(&mut src, &sent, &store);
    assert!(
        src.take_chat_media()
            .iter()
            .all(|u| u.slot != MediaSlot::Viewer),
        "the viewer's album still the same after the grid closed"
    );
}
