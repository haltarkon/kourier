//! The zones of the phone's chat settings (W6a1, A4): what a mapped app-state event writes and tells, what a chat's first
//! statement carries, and the unarchive of a counting message. Over a real `backend.db`; synthetic ids only (the
//! fictitious 555-01xx range: tails of 8 digits that are substrings of no other value).

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_db::DbKey;
use kourier_model::{
    Body, CallDirection, CallLine, CallState, ConvKey, ConversationPatch, Origin, Patch,
    SystemEvent, UserId,
};

use super::{
    Reconciled, after_history, apply, prepare, reconcile, reconcile_with, settings_of,
    with_unarchive,
};
use crate::appstate::map::{self, Mapped, Mirror, Setting};
use crate::features;
use crate::send::started;
use crate::store::chat_state::{self, Field, Put};
use crate::store::{KourierStore, StoreOwner};

const FIRST: &str = "15550148296037";
const SECOND: &str = "15550172940518";
const UNKNOWN: &str = "15550136072849";

fn direct(value: &str) -> ConvKey {
    ConvKey::Direct(UserId::from_parts("lid", value).expect("a user"))
}

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([6; 32]),
        "wa-zone",
    )
    .expect("open");
    (dir, owner)
}

/// The core knows `chat`: its features were stated.
async fn known(store: &KourierStore, chat: &ConvKey) {
    let chat = chat.clone();
    store
        .write("known", move |tx| {
            started::mark_stated(tx, &chat, features::VERSION, true)
        })
        .await
        .expect("mark stated");
}

fn setting(chat: &ConvKey, field: Field, value: i64, at: i64) -> Mapped {
    Mapped {
        collection: match field {
            Field::Muted => "regular_high",
            _ => map::ARCHIVE,
        },
        mirror: Some(Mirror::Chat(Setting {
            chat: chat.clone(),
            field,
            value,
            action_ms: at,
        })),
        events: Vec::new(),
        from_full_sync: false,
    }
}

fn pin(chat: &ConvKey, pinned: bool, at: i64) -> Mapped {
    setting(chat, Field::Pinned, i64::from(pinned), at)
}

async fn run(store: &KourierStore, mapped: Mapped) -> Vec<BackendEvent> {
    store
        .write("apply", move |tx| apply(tx, &mapped))
        .await
        .expect("apply")
}

fn upsert(chat: &ConvKey, edit: impl FnOnce(&mut ConversationPatch)) -> BackendEvent {
    let mut patch = ConversationPatch::new(chat.clone());
    edit(&mut patch);
    BackendEvent::ConversationUpsert(patch)
}

fn pinned_list(chats: &[&ConvKey]) -> BackendEvent {
    BackendEvent::PinnedConversations(chats.iter().map(|c| (*c).clone()).collect())
}

fn from_peer(conv: &ConvKey, id: &str, read: bool) -> BackendEvent {
    let mut message = IncomingMessage::new(
        conv.clone(),
        id,
        Origin::Remote {
            author: conv.direct_peer().cloned().expect("a direct chat"),
            device: None,
        },
        Body::text("hi"),
        1_000,
        1_000,
    );
    message.read = read;
    BackendEvent::Message(message)
}

/// A message of the peer that counts, with the sent time `at` (and another receive time, the stand-in for "now").
fn heard_at(conv: &ConvKey, id: &str, at: i64) -> BackendEvent {
    BackendEvent::Message(IncomingMessage::new(
        conv.clone(),
        id,
        Origin::Remote {
            author: conv.direct_peer().cloned().expect("a direct chat"),
            device: None,
        },
        Body::text("hi"),
        at,
        at + 777_000,
    ))
}

/// The phone's word on "Keep chats archived": `says` = a new message unarchives.
fn unarchive_says(says: bool) -> Mapped {
    Mapped {
        collection: map::ARCHIVE,
        mirror: Some(Mirror::Unarchive(says)),
        events: Vec::new(),
        from_full_sync: false,
    }
}

/// The zone of one live row: the time noted, the chats stated, the unarchive. The events as written.
async fn deliver(store: &KourierStore, events: Vec<BackendEvent>) -> Vec<BackendEvent> {
    store
        .write("deliver", move |tx| {
            started::note_messages(tx, &events)?;
            Ok(prepare(tx, &events)?.unwrap_or(events))
        })
        .await
        .expect("prepare")
}

/// (value, time) of the mirror's archive row of `chat`.
async fn mirror_archive(store: &KourierStore, chat: &ConvKey) -> Option<(i64, i64)> {
    let chat = chat.clone();
    store
        .read(move |conn| chat_state::get(conn, &chat, Field::Archived))
        .await
        .expect("get")
        .map(|row| (row.value, row.action_ms))
}

/// A value the core already has gives no event, a change gives one in the core's words, and the events of the map
/// itself (a clear, a read marker) follow the chat's own. This is what makes a replay of a whole collection reach
/// `core_outbox` only where something changed.
#[tokio::test]
async fn a_value_the_core_has_gives_no_event_and_a_change_gives_one() {
    let (_dir, owner) = open();
    let store = owner.store();
    let chat = direct(FIRST);
    known(&store, &chat).await;

    let archive = |value| setting(&chat, Field::Archived, value, 10);
    assert_eq!(
        run(&store, archive(1)).await,
        [upsert(&chat, |p| p.archived = Some(true))]
    );
    assert!(
        run(&store, archive(1)).await.is_empty(),
        "the same value again says nothing"
    );
    assert_eq!(
        run(&store, archive(0)).await,
        [upsert(&chat, |p| p.archived = Some(false))]
    );

    let mute = |value| setting(&chat, Field::Muted, value, 11);
    assert_eq!(
        run(&store, mute(i64::MAX)).await,
        [upsert(&chat, |p| p.muted_until = Patch::Set(i64::MAX))]
    );
    assert!(run(&store, mute(i64::MAX)).await.is_empty());
    assert_eq!(
        run(&store, mute(0)).await,
        [upsert(&chat, |p| p.muted_until = Patch::Clear)]
    );

    // The map's own events come after the mirror's, and are written whether or not the mirror changed (the core's claim
    // only rises, so a replay of them changes nothing there).
    let clear = BackendEvent::ConversationCleared {
        conv: chat.clone(),
        up_to: Vec::new(),
        remove: false,
        before_ms: Some(5_000),
        at: 12,
    };
    let mut read = setting(&chat, Field::MarkedUnread, 0, 12);
    read.events = vec![clear.clone()];
    assert_eq!(
        run(&store, read.clone()).await,
        std::slice::from_ref(&clear)
    );
    let mut unread = setting(&chat, Field::MarkedUnread, 1, 13);
    unread.events = vec![clear.clone()];
    assert_eq!(
        run(&store, unread).await,
        [
            upsert(&chat, |p| p.marked_unread = Some(true)),
            clear.clone()
        ]
    );
    assert_eq!(
        run(&store, read).await,
        [upsert(&chat, |p| p.marked_unread = Some(false)), clear]
    );
    owner.close();
}

/// The pin list is the FULL list of the pins of chats the core knows, newest first: one pin needs the others, a pin of an
/// unknown chat stays out, a chat pinned again moves to the front, and unpinning what was never pinned says nothing.
#[tokio::test]
async fn the_pin_list_is_every_known_pin_newest_first() {
    let (_dir, owner) = open();
    let store = owner.store();
    let (first, second, unknown) = (direct(FIRST), direct(SECOND), direct(UNKNOWN));
    known(&store, &first).await;
    known(&store, &second).await;

    assert_eq!(
        run(&store, pin(&first, true, 100)).await,
        [pinned_list(&[&first])]
    );
    assert_eq!(
        run(&store, pin(&second, true, 200)).await,
        [pinned_list(&[&second, &first])],
        "the whole list, the newer pin first"
    );
    assert!(
        run(&store, pin(&unknown, true, 300)).await.is_empty(),
        "a pin of a chat the core does not know is not told (H5)"
    );
    assert_eq!(
        run(&store, pin(&first, true, 400)).await,
        [pinned_list(&[&first, &second])],
        "pinned again = the new place in the order"
    );
    assert!(
        run(&store, pin(&first, true, 400)).await.is_empty(),
        "the same pin again changes nothing"
    );
    assert_eq!(
        run(&store, pin(&second, false, 500)).await,
        [pinned_list(&[&first])]
    );
    assert!(
        run(&store, pin(&second, false, 600)).await.is_empty(),
        "unpinning what is not pinned changes nothing, whatever the time"
    );
    owner.close();
}

/// H5: the phone's values for a chat the core does not know are kept and tell nobody; the zone that first states the
/// chat states them, in order, right after the chat's features and before its first event, and the pin list with them.
/// After that a replay says nothing.
#[tokio::test]
async fn what_the_phone_said_of_an_unknown_chat_is_stated_with_the_chat_in_order() {
    let (_dir, owner) = open();
    let store = owner.store();
    let chat = direct(UNKNOWN);
    let other = direct(FIRST);
    known(&store, &other).await;
    run(&store, pin(&other, true, 50)).await;

    for mapped in [
        setting(&chat, Field::Archived, 1, 10),
        setting(&chat, Field::Muted, i64::MAX, 11),
        setting(&chat, Field::MarkedUnread, 1, 12),
        pin(&chat, true, 100),
    ] {
        assert!(
            run(&store, mapped).await.is_empty(),
            "nothing is told about a chat the core does not know"
        );
    }
    // The chat's first event: a message from the peer.
    let first = from_peer(&chat, "M1", false);
    let events = vec![first.clone()];
    let written = store
        .write("first message", move |tx| {
            started::note_messages(tx, &events)?;
            prepare(tx, &events)
        })
        .await
        .expect("prepare")
        .expect("the chat is stated");
    let kinds: Vec<String> = written
        .iter()
        .map(|event| match event {
            BackendEvent::ConversationUpsert(patch) if features::is_statement(event) => {
                format!("statement {}", patch.key == chat)
            }
            BackendEvent::ConversationUpsert(patch) => format!(
                "upsert archived={:?} unread={:?} muted={:?}",
                patch.archived, patch.marked_unread, patch.muted_until
            ),
            BackendEvent::PinnedConversations(list) => format!("pins {}", list.len()),
            BackendEvent::Message(_) => "message".to_owned(),
            _ => "other".to_owned(),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "statement true".to_owned(),
            "upsert archived=Some(true) unread=None muted=Unchanged".to_owned(),
            "upsert archived=None unread=Some(true) muted=Unchanged".to_owned(),
            format!(
                "upsert archived=None unread=None muted={:?}",
                Patch::Set(i64::MAX)
            ),
            "pins 2".to_owned(),
            "message".to_owned(),
        ],
        "the statement, the waiting values, the full pin list, then the message"
    );
    assert_eq!(
        written.iter().find_map(|e| match e {
            BackendEvent::PinnedConversations(list) => Some(list.clone()),
            _ => None,
        }),
        Some(vec![chat.clone(), other.clone()]),
        "the new pin leads the other"
    );
    assert_eq!(written.last(), Some(&first));

    // Stated once: a second message, a replay of the same values and the sweep say nothing more.
    let again = vec![from_peer(&chat, "M2", false)];
    let second = store
        .write("second message", move |tx| prepare(tx, &again))
        .await
        .expect("prepare");
    assert_eq!(second, None);
    assert!(
        run(&store, setting(&chat, Field::Archived, 1, 10))
            .await
            .is_empty(),
        "a stated value is the core's now"
    );
    assert!(
        store
            .read({
                let chat = chat.clone();
                move |conn| chat_state::to_state(conn, &chat)
            })
            .await
            .expect("to state")
            .is_empty()
    );
    owner.close();
}

/// A chat whose rows are all at the default needs nothing stated with it (the usual chat): no extra event, no pin list.
#[tokio::test]
async fn a_chat_the_phone_said_nothing_about_gets_nothing_with_its_statement() {
    let (_dir, owner) = open();
    let store = owner.store();
    let chat = direct(UNKNOWN);
    run(&store, setting(&chat, Field::Archived, 0, 10)).await;
    run(&store, pin(&chat, false, 11)).await;
    let events = store
        .write("settings", {
            let chat = chat.clone();
            move |tx| settings_of(tx, &chat)
        })
        .await
        .expect("settings");
    assert!(events.is_empty(), "{events:?}");
    owner.close();
}

/// H6: a counting message in an archived chat brings it back when the phone's setting says so, and not otherwise. The
/// setting is the phone's word (`setting_unarchiveChats`): never said, or said "keep archived", leaves the chat in
/// the archive. A message that does not count (read on arrival, a notice, a send of this device) never brings it back.
#[tokio::test]
async fn a_counting_message_unarchives_when_the_setting_says_so_and_not_otherwise() {
    let (_dir, owner) = open();
    let store = owner.store();
    let chat = direct(FIRST);
    known(&store, &chat).await;
    run(&store, setting(&chat, Field::Archived, 1, 10)).await;

    let try_with = |event: BackendEvent| {
        let store = store.clone();
        async move {
            store
                .write("unarchive", move |tx| with_unarchive(tx, &[event]))
                .await
                .expect("prepare")
        }
    };
    let counting = || from_peer(&chat, "M1", false);
    let flag = |says: bool| Mapped {
        collection: "regular_low",
        mirror: Some(Mirror::Unarchive(says)),
        events: Vec::new(),
        from_full_sync: false,
    };

    assert_eq!(try_with(counting()).await, None, "the phone never said");
    run(&store, flag(false)).await;
    assert_eq!(try_with(counting()).await, None, "keep chats archived");

    run(&store, flag(true)).await;
    // What does not count.
    let mut own_here = IncomingMessage::new(
        chat.clone(),
        "M2",
        Origin::OwnThisDevice,
        Body::text("sent here"),
        1_000,
        1_000,
    );
    own_here.read = true;
    for (name, event) in [
        ("a message read on arrival", from_peer(&chat, "M3", true)),
        (
            "a notice",
            BackendEvent::Message(IncomingMessage::new(
                chat.clone(),
                "M4",
                Origin::Remote {
                    author: chat.direct_peer().cloned().expect("peer"),
                    device: None,
                },
                Body::System(SystemEvent::Joined),
                1_000,
                1_000,
            )),
        ),
        ("a send of this device", BackendEvent::Message(own_here)),
    ] {
        assert_eq!(try_with(event).await, None, "{name} does not count");
    }

    let message = counting();
    let written = try_with(message.clone()).await.expect("brought back");
    assert_eq!(
        written,
        [upsert(&chat, |p| p.archived = Some(false)), message],
        "the unarchive goes in front of the message"
    );
    assert_eq!(
        try_with(counting()).await,
        None,
        "once back, the next message has nothing to bring back"
    );
    // The phone's own archive record afterwards is the core's news again.
    assert_eq!(
        run(&store, setting(&chat, Field::Archived, 1, 20)).await,
        [upsert(&chat, |p| p.archived = Some(true))]
    );
    owner.close();
}

/// A chat that the phone archived and the core has never heard of is stated by the zone that first states it. With
/// "unarchive" on, an archive older than the message that states the chat is stale (F1): the chat is stated NOT archived,
/// with no archived word at all; an archive that is as new as the message (the last message of its range) holds, and the
/// message does not bring it back.
#[tokio::test]
async fn a_chat_first_stated_with_an_older_archive_is_not_archived() {
    let (_dir, owner) = open();
    let store = owner.store();
    let chat = direct(UNKNOWN);
    let same = direct(SECOND);
    run(&store, setting(&chat, Field::Archived, 1, 500)).await;
    run(&store, setting(&same, Field::Archived, 1, 1_000)).await;
    run(&store, unarchive_says(true)).await;
    let archived_words = |written: &[BackendEvent]| -> Vec<Option<bool>> {
        written
            .iter()
            .filter_map(|event| match event {
                BackendEvent::ConversationUpsert(patch) if !features::is_statement(event) => {
                    Some(patch.archived)
                }
                _ => None,
            })
            .collect()
    };
    let older = deliver(&store, vec![heard_at(&chat, "M1", 1_000)]).await;
    assert_eq!(
        archived_words(&older),
        Vec::<Option<bool>>::new(),
        "an archive older than the message that states the chat: no archive"
    );
    assert!(matches!(older.last(), Some(BackendEvent::Message(_))));
    assert_eq!(mirror_archive(&store, &chat).await, Some((0, 500)));

    let tie = deliver(&store, vec![heard_at(&same, "M1", 1_000)]).await;
    assert_eq!(
        archived_words(&tie),
        [Some(true)],
        "an archive of the very time of the message holds, and the message does not bring it back"
    );
    assert_eq!(mirror_archive(&store, &same).await, Some((1, 1_000)));
    owner.close();
}

/// Writes one row of the mirror as a replay or a live event would (a generation, a collection, stated or not).
async fn row(
    store: &KourierStore,
    chat: &ConvKey,
    field: Field,
    value: i64,
    generation: i64,
    stated: bool,
) {
    let chat = chat.clone();
    store
        .write("row", move |tx| {
            chat_state::put(
                tx,
                &Put {
                    chat: &chat,
                    field,
                    value,
                    action_ms: 100 + generation,
                    generation,
                    collection: match field {
                        Field::Muted => "regular_high",
                        _ => "regular_low",
                    },
                    stated,
                },
            )
            .map(drop)
        })
        .await
        .expect("put");
}

/// The reconcile (A5b, H1): after the snapshot of generation 1 of `regular_low`, the rows of THAT collection left on an
/// older generation and not at the default go back to it, in the core's words, and the full pin list follows when a
/// pin went; a row the replay touched (generation 1), a row of another collection and a row already at the default
/// stay; a value the core never heard (an unknown chat) is reset in the mirror alone. Kills "the pin list not restated",
/// "a row of the generation reset too" (`<=`), "the collection ignored", "a row at the default counted" and "an unstated
/// value told to the core".
#[tokio::test]
async fn the_rows_a_snapshot_left_unmentioned_go_back_to_their_defaults_in_the_cores_words() {
    let (_dir, owner) = open();
    let store = owner.store();
    let (gone, seen, muted, stranger) = (
        direct(FIRST),
        direct(SECOND),
        direct("15550193820746"),
        direct(UNKNOWN),
    );
    for chat in [&gone, &seen, &muted] {
        known(&store, chat).await;
    }
    // Before the snapshot (generation 0): `gone` is archived and pinned, and so is the stranger's archive (unstated);
    // the snapshot (generation 1) mentioned `seen`'s pin and archive; `muted` is muted in the other collection; an
    // archive at the default does not count.
    row(&store, &gone, Field::Archived, 1, 0, true).await;
    row(&store, &gone, Field::Pinned, 1, 0, true).await;
    row(&store, &stranger, Field::Archived, 1, 0, false).await;
    row(&store, &seen, Field::Pinned, 1, 1, true).await;
    row(&store, &seen, Field::Archived, 1, 1, true).await;
    row(&store, &muted, Field::Muted, i64::MAX, 0, true).await;
    row(&store, &muted, Field::MarkedUnread, 0, 0, true).await;

    let done = store
        .write("reconcile", |tx| reconcile(tx, "regular_low", 1))
        .await
        .expect("reconcile");
    assert_eq!(
        done,
        Reconciled {
            events: vec![
                upsert(&gone, |p| p.archived = Some(false)),
                pinned_list(&[&seen]),
            ],
            reset: 3,
            kept: 0,
        }
    );
    let value = |chat: &ConvKey, field| {
        let chat = chat.clone();
        let store = store.clone();
        async move {
            store
                .read(move |conn| chat_state::get(conn, &chat, field))
                .await
                .expect("get")
                .map(|row| (row.value, row.generation, row.stated))
        }
    };
    assert_eq!(value(&gone, Field::Archived).await, Some((0, 1, true)));
    assert_eq!(value(&gone, Field::Pinned).await, Some((0, 1, true)));
    assert_eq!(
        value(&stranger, Field::Archived).await,
        Some((0, 1, false)),
        "the mirror alone goes back, nobody is told"
    );
    assert_eq!(value(&seen, Field::Pinned).await, Some((1, 1, true)));
    assert_eq!(value(&seen, Field::Archived).await, Some((1, 1, true)));
    assert_eq!(
        value(&muted, Field::Muted).await,
        Some((i64::MAX, 0, true)),
        "regular_high is another collection"
    );

    // A second reconcile finds nothing: the rows are at their defaults and on the generation.
    let again = store
        .write("reconcile", |tx| reconcile(tx, "regular_low", 1))
        .await
        .expect("again");
    assert_eq!(again, Reconciled::default());
    owner.close();
}

/// M8: a row with an open intent of W6b is never reset, and the rest of the collection is. The hook `has_open_intent`
/// is false until W6b gives it a table; the question is asked per row. Kills "the intent is not asked".
#[tokio::test]
async fn a_row_with_an_open_intent_is_not_reset() {
    let (_dir, owner) = open();
    let store = owner.store();
    let (kept, reset) = (direct(FIRST), direct(SECOND));
    known(&store, &kept).await;
    known(&store, &reset).await;
    row(&store, &kept, Field::Muted, i64::MAX, 0, true).await;
    row(&store, &reset, Field::Muted, i64::MAX, 0, true).await;

    let intent_chat = kept.clone();
    let done = store
        .write("reconcile", move |tx| {
            reconcile_with(tx, "regular_high", 1, &|_, chat, field| {
                Ok(*chat == intent_chat && field == Field::Muted)
            })
        })
        .await
        .expect("reconcile");
    assert_eq!(
        done,
        Reconciled {
            events: vec![upsert(&reset, |p| p.muted_until = Patch::Clear)],
            reset: 1,
            kept: 1,
        }
    );
    let still = store
        .read(move |conn| chat_state::get(conn, &kept, Field::Muted))
        .await
        .expect("get")
        .expect("a row");
    assert_eq!((still.value, still.generation), (i64::MAX, 0));
    owner.close();
}

// The address book's reconcile (F19, DUR-6). Contacts of the fictitious 555-06xx range; tails of 8 digits that are substrings
// of no other value.
const OLD_BOSS: &str = "15550611201";
const ANN: &str = "15550611202";
const CAT: &str = "15550611203";
const DAN: &str = "15550611204";
const EVE: &str = "15550611205";

/// The phone's `contact` mutation naming the number `pn` with `name`.
fn contact(pn: &str, name: &str) -> crate::book::Change {
    let mut action = waproto::whatsapp::sync_action_value::ContactAction::default();
    action.full_name = Some(name.to_owned());
    crate::book::change_of_action(
        &format!("{pn}@s.whatsapp.net").parse().expect("jid"),
        &action,
    )
    .expect("a change")
}

fn person(pn: &str) -> UserId {
    UserId::from_parts("pn", pn).expect("a user")
}

/// What the core is told when the phone no longer has a contact whose number it was told with his name.
fn name_taken_back(pn: &str) -> BackendEvent {
    let mut patch = kourier_model::ContactPatch::new(person(pn));
    patch.system_name = Patch::Clear;
    patch.identifiers = Some(Vec::new());
    patch.number_shown = Some(false);
    BackendEvent::ContactUpsert(patch)
}

/// F19 (DUR-6): after the snapshot of generation 1 of `critical_unblock_low`, the contacts the app state said that it left
/// on an older generation are what the phone no longer has: the mirror drops them and the core is told, as a REMOVE tells
/// it (and only for a person it knows). A contact the replay said again (even with the same word: a touch) and one it
/// renamed stay, a history contact has no generation and stays, another collection's reconcile leaves the book alone, and
/// a second reconcile finds nothing. Kills "the book left out of the reconcile", "the history's rows reset too" (the
/// origin), "a row of the generation reset too" (`<=`) and "a replay of the same word is no touch".
#[tokio::test]
async fn the_contacts_a_snapshot_left_unmentioned_leave_the_address_book_and_the_core() {
    let (_dir, owner) = open();
    let store = owner.store();
    // The core knows all but Eve. Before the snapshot (generation 0) the phone said the first four and Eve; the history
    // said Dan.
    for pn in [OLD_BOSS, ANN, CAT, DAN] {
        let id = person(pn);
        store
            .write("known", move |tx| {
                crate::store::book::know(tx, id.as_str()).map(drop)
            })
            .await
            .expect("know");
    }
    let said = |change: crate::book::Change, generation: i64| {
        let store = store.clone();
        async move {
            store
                .write("said", move |tx| {
                    crate::book::apply(tx, &change, generation)
                })
                .await
                .expect("apply")
        }
    };
    for (pn, name) in [
        (OLD_BOSS, "Old Boss"),
        (ANN, "Ann"),
        (CAT, "Cat"),
        (EVE, "Eve"),
    ] {
        said(contact(pn, name), 0).await;
    }
    store
        .write("older", |tx| {
            crate::book::apply_older(tx, &contact(DAN, "Dan"))
        })
        .await
        .expect("history");
    // The snapshot (generation 1) says Ann again (nothing new: no event) and renames Cat.
    assert!(said(contact(ANN, "Ann"), 1).await.is_empty());
    assert_eq!(said(contact(CAT, "Cat New"), 1).await.len(), 1);

    let held = |pn: &'static str| {
        let store = store.clone();
        async move {
            store
                .read(move |conn| crate::store::book::of(conn, person(pn).as_str()))
                .await
                .expect("of")
                .map(|row| row.name)
        }
    };
    // Another collection's reconcile does not touch the book.
    let other = store
        .write("reconcile", |tx| reconcile(tx, "regular_low", 1))
        .await
        .expect("other");
    assert_eq!(other, Reconciled::default());
    assert_eq!(held(OLD_BOSS).await.as_deref(), Some("Old Boss"));

    let done = store
        .write("reconcile", |tx| reconcile(tx, map::CONTACT, 1))
        .await
        .expect("reconcile");
    assert_eq!(
        done,
        Reconciled {
            events: vec![name_taken_back(OLD_BOSS)],
            reset: 2,
            kept: 0,
        },
        "Old Boss and the stranger Eve went; only the person the core knows is told"
    );
    assert_eq!(held(OLD_BOSS).await, None);
    assert_eq!(
        held(EVE).await,
        None,
        "the stranger's row went with no event"
    );
    assert_eq!(held(ANN).await.as_deref(), Some("Ann"));
    assert_eq!(held(CAT).await.as_deref(), Some("Cat New"));
    assert_eq!(
        held(DAN).await.as_deref(),
        Some("Dan"),
        "a history row is not the collection's"
    );
    let boss_name = person(OLD_BOSS);
    let stated = store
        .read(move |conn| {
            crate::store::people::name_of(
                conn,
                boss_name.as_str(),
                crate::store::people::NameKind::System,
            )
        })
        .await
        .expect("name_of");
    assert_eq!(stated, None, "backend.db forgot the name it told");

    let again = store
        .write("reconcile", |tx| reconcile(tx, map::CONTACT, 1))
        .await
        .expect("again");
    assert_eq!(again, Reconciled::default());
    owner.close();
}

/// The ids of the upload records the store holds, sorted.
async fn upload_records(store: &KourierStore) -> Vec<String> {
    store
        .read(|conn| {
            let mut stmt = conn
                .prepare("SELECT local FROM media_uploads ORDER BY local")
                .map_err(crate::error::sql("read uploads"))?;
            stmt.query_map([], |row| row.get::<_, String>(0))
                .map_err(crate::error::sql("read uploads"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(crate::error::sql("read uploads"))
        })
        .await
        .expect("read uploads")
}

/// DUR-10 (F8): the phone's own clear or delete of a chat forgets the records of Kourier's uploads in THAT chat, in the
/// same zone, as Kourier's own clear does; another chat's records and a delete-for-me of one message leave them alone.
/// Kills "the phone's clear leaves the records" and "every chat's records go".
#[tokio::test]
async fn the_phones_clear_or_delete_forgets_the_chats_upload_records() {
    let (_dir, owner) = open();
    let store = owner.store();
    let (first, second) = (direct(FIRST), direct(SECOND));
    let seeded = [
        ("3EB0F8A100000001", first.encode()),
        ("3EB0F8A100000002", first.encode()),
        ("3EB0F8A100000003", second.encode()),
    ];
    store
        .write("seed uploads", move |tx| {
            for (local, conv) in &seeded {
                tx.execute(
                    "INSERT INTO media_uploads(local, conv, pointer, at) VALUES (?1, ?2, x'7b7d', 1)",
                    (local, conv),
                )
                .map_err(crate::error::sql("seed"))?;
            }
            Ok(())
        })
        .await
        .expect("seed");
    let said = |event: BackendEvent| Mapped {
        collection: "regular_high",
        mirror: None,
        events: vec![event],
        from_full_sync: false,
    };
    let cleared = |conv: &ConvKey, remove: bool| BackendEvent::ConversationCleared {
        conv: conv.clone(),
        up_to: Vec::new(),
        remove,
        before_ms: Some(5_000),
        at: 12,
    };

    // A delete-for-me of one message is not a clear.
    let gone = BackendEvent::ReadMarker {
        conv: Some(first.clone()),
        up_to: kourier_model::MsgTarget {
            conv: Some(first.clone()),
            author: kourier_model::TargetAuthor::Own,
            local: "3EB0F8A100000001".into(),
            alternates: Vec::new(),
        },
    };
    run(&store, said(gone)).await;
    assert_eq!(
        upload_records(&store).await.len(),
        3,
        "a read marker forgets nothing"
    );

    let told = run(&store, said(cleared(&first, false))).await;
    assert_eq!(
        told,
        [cleared(&first, false)],
        "the core still hears the clear"
    );
    assert_eq!(
        upload_records(&store).await,
        ["3EB0F8A100000003"],
        "the first chat's records are gone, the second chat's stay"
    );
    run(&store, said(cleared(&second, true))).await;
    assert!(
        upload_records(&store).await.is_empty(),
        "a delete forgets them too"
    );
    owner.close();
}

/// NEW-4 (G4): the phone's clear BY A RANGE time removes the messages up to the range's end; a file Kourier sent after it
/// is still in the chat, so its upload record stays. A record at or before the end goes; a clear that names no time and a
/// delete of the chat forget every record of the chat; another chat's records are never touched.
/// Kills "the bound dropped" (the later record forgotten) and "the bound off by one" (a record AT the end kept).
#[tokio::test]
async fn the_phones_ranged_clear_keeps_the_upload_records_after_its_range() {
    let seed = |store: KourierStore| async move {
        let (first, second) = (direct(FIRST), direct(SECOND));
        let seeded = [
            ("3EB0F8A100000011", first.encode(), 1_000),
            ("3EB0F8A100000012", first.encode(), 5_000),
            ("3EB0F8A100000013", first.encode(), 5_001),
            ("3EB0F8A100000014", first.encode(), 9_000),
            ("3EB0F8A100000015", second.encode(), 1_000),
        ];
        store
            .write("seed uploads", move |tx| {
                for (local, conv, at) in &seeded {
                    tx.execute(
                        "INSERT INTO media_uploads(local, conv, pointer, at) VALUES (?1, ?2, x'7b7d', ?3)",
                        (local, conv, at),
                    )
                    .map_err(crate::error::sql("seed"))?;
                }
                Ok(())
            })
            .await
            .expect("seed");
    };
    let said = |remove: bool, before_ms: Option<i64>| Mapped {
        collection: "regular_high",
        mirror: None,
        events: vec![BackendEvent::ConversationCleared {
            conv: direct(FIRST),
            up_to: Vec::new(),
            remove,
            before_ms,
            at: 12,
        }],
        from_full_sync: false,
    };

    // A clear by the range ending at 5_000: the records at or before it go, the later ones stay.
    let (_dir, owner) = open();
    let store = owner.store();
    seed(store.clone()).await;
    run(&store, said(false, Some(5_000))).await;
    assert_eq!(
        upload_records(&store).await,
        ["3EB0F8A100000013", "3EB0F8A100000014", "3EB0F8A100000015"],
        "the clear kept what was uploaded after its range, and the other chat's record"
    );
    owner.close();

    // A clear that names no time, and a delete of the chat, forget every record of the chat.
    for (remove, before_ms) in [(false, None), (true, Some(5_000)), (true, None)] {
        let (_dir, owner) = open();
        let store = owner.store();
        seed(store.clone()).await;
        run(&store, said(remove, before_ms)).await;
        assert_eq!(
            upload_records(&store).await,
            ["3EB0F8A100000015"],
            "remove {remove}, range {before_ms:?}: all of the chat's records go"
        );
        owner.close();
    }
}

/// INTEROP-4 (F3): the phone's clear or delete of a chat notes the chat's CUT in the same zone (the range's time, else the
/// action's own), only ever raised; a delete-for-me of one message or a read marker sets none. Kills "the zone notes no
/// cut" (a call of the log after a delete would never bring the chat back).
#[tokio::test]
async fn the_phones_clear_or_delete_notes_the_chats_cut() {
    let (_dir, owner) = open();
    let store = owner.store();
    let (first, second, third) = (direct(FIRST), direct(SECOND), direct(UNKNOWN));
    let said = |event: BackendEvent| Mapped {
        collection: "regular_high",
        mirror: None,
        events: vec![event],
        from_full_sync: false,
    };
    let cleared = |conv: &ConvKey, remove: bool, before_ms: Option<i64>, at: i64| {
        BackendEvent::ConversationCleared {
            conv: conv.clone(),
            up_to: Vec::new(),
            remove,
            before_ms,
            at,
        }
    };
    let cut_of = |chat: ConvKey| {
        let store = store.clone();
        async move {
            store
                .read(move |conn| chat_state::cut_of(conn, &chat))
                .await
                .expect("cut_of")
        }
    };

    run(&store, said(cleared(&first, false, Some(5_000), 9_000))).await;
    assert_eq!(cut_of(first.clone()).await, Some(5_000), "the range's time");
    run(&store, said(cleared(&second, true, None, 7_000))).await;
    assert_eq!(
        cut_of(second.clone()).await,
        Some(7_000),
        "a delete that names messages only: the action's own time"
    );
    run(&store, said(cleared(&first, true, Some(3_000), 9_500))).await;
    assert_eq!(
        cut_of(first.clone()).await,
        Some(5_000),
        "an older claim lowers nothing"
    );
    run(&store, said(cleared(&first, true, Some(8_000), 9_600))).await;
    assert_eq!(cut_of(first).await, Some(8_000));
    let marker = BackendEvent::ReadMarker {
        conv: Some(third.clone()),
        up_to: kourier_model::MsgTarget {
            conv: Some(third.clone()),
            author: kourier_model::TargetAuthor::Own,
            local: "3EB0F8A100000009".into(),
            alternates: Vec::new(),
        },
    };
    run(&store, said(marker)).await;
    assert_eq!(cut_of(third).await, None, "a read marker is no clear");
    owner.close();
}

/// INTEROP-1 (F1): the phone writes nothing when its "Keep chats archived" OFF moves a chat out of the archive, so a
/// replay (the link's full sync, a snapshot) hands over the archive record of a chat that wrote after it. With the
/// setting at "unarchive" such a record is stale and the chat is NOT archived; with "keep chats archived" it holds.
#[tokio::test]
async fn an_archive_older_than_the_chats_last_message_is_stale_only_while_the_phone_unarchives() {
    for says in [true, false] {
        let (_dir, owner) = open();
        let store = owner.store();
        let chat = direct(FIRST);
        known(&store, &chat).await;
        run(&store, unarchive_says(says)).await;
        // The chat wrote at 2_000; the record is from 1_000.
        deliver(&store, vec![heard_at(&chat, "M1", 2_000)]).await;
        let told = run(&store, setting(&chat, Field::Archived, 1, 1_000)).await;
        if says {
            assert!(told.is_empty(), "a stale archive tells the core nothing");
            assert_eq!(mirror_archive(&store, &chat).await, Some((0, 1_000)));
        } else {
            assert_eq!(told, [upsert(&chat, |p| p.archived = Some(true))]);
            assert_eq!(mirror_archive(&store, &chat).await, Some((1, 1_000)));
        }
        owner.close();
    }
}

/// DUR-3 (a), both orders (F1): the user archives a chat on the phone after its last message. The archive's range ends
/// at that message, so the message has the archive's own time: it brings nothing back whether it reaches Kourier before
/// the archive or after, and one that is really newer does. An archive newer than the last message holds.
#[tokio::test]
async fn the_phone_archiving_after_the_last_message_holds_in_both_orders() {
    // The archive first, then the message of its range, then a newer one.
    {
        let (_dir, owner) = open();
        let store = owner.store();
        let chat = direct(FIRST);
        known(&store, &chat).await;
        run(&store, unarchive_says(true)).await;
        let told = run(&store, setting(&chat, Field::Archived, 1, 1_000)).await;
        assert_eq!(told, [upsert(&chat, |p| p.archived = Some(true))]);
        let same_time = heard_at(&chat, "M1", 1_000);
        assert_eq!(
            deliver(&store, vec![same_time.clone()]).await,
            [same_time],
            "the message the archive's range ends at brings nothing back"
        );
        assert_eq!(mirror_archive(&store, &chat).await, Some((1, 1_000)));
        let newer = heard_at(&chat, "M2", 1_001);
        assert_eq!(
            deliver(&store, vec![newer.clone()]).await,
            [upsert(&chat, |p| p.archived = Some(false)), newer],
            "a message after the archive does"
        );
    }
    // The message first, then the archive of its range, or a newer one.
    for archived_at in [1_000, 5_000] {
        let (_dir, owner) = open();
        let store = owner.store();
        let chat = direct(FIRST);
        known(&store, &chat).await;
        run(&store, unarchive_says(true)).await;
        deliver(&store, vec![heard_at(&chat, "M1", 1_000)]).await;
        let told = run(&store, setting(&chat, Field::Archived, 1, archived_at)).await;
        assert_eq!(
            told,
            [upsert(&chat, |p| p.archived = Some(true))],
            "an archive dated {archived_at} after a message at 1000 holds"
        );
        owner.close();
    }
}

/// A message that brings a chat back stamps the archive row with ITS time (F1), and the older archive record, replayed
/// afterwards, changes nothing: the chat stays out of the archive and the core hears no second word.
#[tokio::test]
async fn a_replay_of_the_older_archive_after_a_local_unarchive_changes_nothing() {
    let (_dir, owner) = open();
    let store = owner.store();
    let chat = direct(FIRST);
    known(&store, &chat).await;
    run(&store, unarchive_says(true)).await;
    run(&store, setting(&chat, Field::Archived, 1, 1_000)).await;
    let message = heard_at(&chat, "M1", 2_000);
    assert_eq!(
        deliver(&store, vec![message.clone()]).await,
        [upsert(&chat, |p| p.archived = Some(false)), message]
    );
    assert_eq!(
        mirror_archive(&store, &chat).await,
        Some((0, 2_000)),
        "the row takes the message's time, not the receive time"
    );
    assert!(
        run(&store, setting(&chat, Field::Archived, 1, 1_000))
            .await
            .is_empty(),
        "the replay of the older record changes nothing"
    );
    assert_eq!(
        mirror_archive(&store, &chat).await.map(|row| row.0),
        Some(0)
    );
    owner.close();
}

/// The setting and the archive records live in one collection, in an order nobody controls: a snapshot may hand over
/// the archive first (the setting not known yet, so it holds) and "unarchive" afterwards. The setting then takes back
/// the archives a newer message made stale, tells the core, and leaves a newer archive alone (F1).
#[tokio::test]
async fn the_setting_arriving_after_the_archive_records_takes_back_the_stale_ones() {
    let (_dir, owner) = open();
    let store = owner.store();
    let (stale, fresh) = (direct(FIRST), direct(SECOND));
    known(&store, &stale).await;
    known(&store, &fresh).await;
    for chat in [&stale, &fresh] {
        deliver(&store, vec![heard_at(chat, "M1", 2_000)]).await;
    }
    for (chat, at) in [(&stale, 1_000), (&fresh, 3_000)] {
        assert_eq!(
            run(&store, setting(chat, Field::Archived, 1, at)).await,
            [upsert(chat, |p| p.archived = Some(true))],
            "the setting is not known: the archive holds"
        );
    }
    assert_eq!(
        run(&store, unarchive_says(true)).await,
        [upsert(&stale, |p| p.archived = Some(false))],
        "only the archive a newer message made stale goes"
    );
    assert_eq!(mirror_archive(&store, &stale).await, Some((0, 1_000)));
    assert_eq!(mirror_archive(&store, &fresh).await, Some((1, 3_000)));
    assert!(
        run(&store, unarchive_says(true)).await.is_empty(),
        "the same word again changes nothing"
    );
    owner.close();
}

/// The history delivery's zones note the newest message that counts too (F1), counting a message of somebody else
/// whatever its blanket `read: true` says, and an archive older than it goes. A send of this device does not count.
#[tokio::test]
async fn the_history_notes_its_newest_message_and_takes_back_an_older_archive() {
    let (_dir, owner) = open();
    let store = owner.store();
    let chat = direct(FIRST);
    known(&store, &chat).await;
    run(&store, unarchive_says(true)).await;
    run(&store, setting(&chat, Field::Archived, 1, 1_000)).await;

    let history = |chat: &ConvKey, id: &str, at: i64| -> IncomingMessage {
        let BackendEvent::Message(mut message) = heard_at(chat, id, at) else {
            unreachable!("heard_at is a message");
        };
        message.read = true;
        message
    };
    let after = |messages: Vec<IncomingMessage>| {
        let (store, chat) = (store.clone(), chat.clone());
        async move {
            store
                .write("history", move |tx| after_history(tx, &chat, &messages))
                .await
                .expect("after history")
        }
    };
    let newest = || {
        let (store, chat) = (store.clone(), chat.clone());
        async move {
            store
                .read(move |conn| chat_state::newest_counted(conn, &chat))
                .await
                .expect("newest")
        }
    };
    let mut sent_here = history(&chat, "M0", 9_999);
    sent_here.origin = Origin::OwnThisDevice;
    assert!(after(vec![sent_here]).await.is_empty());
    assert_eq!(newest().await, None, "a send of this device does not count");

    assert!(after(vec![history(&chat, "M1", 500)]).await.is_empty());
    assert_eq!(newest().await, Some(500), "noted, older than the archive");
    assert_eq!(mirror_archive(&store, &chat).await, Some((1, 1_000)));

    assert_eq!(
        after(vec![
            history(&chat, "M2", 1_500),
            history(&chat, "M3", 2_000)
        ])
        .await,
        [upsert(&chat, |p| p.archived = Some(false))],
        "a read row of the push that is newer than the archive: the phone moved the chat out"
    );
    assert_eq!(newest().await, Some(2_000), "the newest of the batch");
    assert_eq!(mirror_archive(&store, &chat).await, Some((0, 1_000)));
    assert!(
        after(vec![history(&chat, "M4", 1_900)]).await.is_empty(),
        "nothing left to take back"
    );
    assert_eq!(
        newest().await,
        Some(2_000),
        "an older row never lowers the time"
    );
    owner.close();
}

/// A ringing call line of call `id` from the peer of `conv`, with the sent time `at`.
fn ring_at(conv: &ConvKey, id: &str, at: i64) -> IncomingMessage {
    let author = conv.direct_peer().cloned().expect("a direct chat");
    IncomingMessage::call(
        conv.clone(),
        id,
        Origin::Remote {
            author: author.clone(),
            device: None,
        },
        CallLine::signal(
            CallDirection::Incoming,
            false,
            CallState::Ringing {
                deadline: at + 60_000,
            },
            at,
        )
        .with_by(author, at),
        at,
        at,
    )
}

/// G1 (NEW-1): the phone dates an archive by the range's last TEXT (`lastMessageTimestamp`), calls being on the range's
/// system side. A call that rang after the last text therefore does not make the phone's archive stale, in the live zone
/// or in the history's: the chat is archived, setting "unarchive" on. A text after the archive still does.
#[tokio::test]
async fn a_call_line_does_not_date_the_chats_last_message_for_the_archive() {
    // The live zone: a text at 1_000, a call at 2_000, then the phone's archive of a range ending at the text.
    {
        let (_dir, owner) = open();
        let store = owner.store();
        let chat = direct(FIRST);
        known(&store, &chat).await;
        run(&store, unarchive_says(true)).await;
        deliver(&store, vec![heard_at(&chat, "M1", 1_000)]).await;
        let call = BackendEvent::Message(ring_at(&chat, "CALL0001", 2_000));
        assert_eq!(
            deliver(&store, vec![call.clone()]).await,
            [call],
            "a call into a chat that is not archived changes nothing"
        );
        assert_eq!(
            run(&store, setting(&chat, Field::Archived, 1, 1_000)).await,
            [upsert(&chat, |p| p.archived = Some(true))],
            "the call after the last text does not make the archive stale"
        );
        assert_eq!(mirror_archive(&store, &chat).await, Some((1, 1_000)));
        // A call into the archived chat still brings it back (A1's rule), a text after the archive does as well.
        let call = BackendEvent::Message(ring_at(&chat, "CALL0002", 3_000));
        assert_eq!(
            deliver(&store, vec![call.clone()]).await,
            [upsert(&chat, |p| p.archived = Some(false)), call]
        );
    }
    // The history's zone: the same, and the noted time stays the text's.
    {
        let (_dir, owner) = open();
        let store = owner.store();
        let chat = direct(FIRST);
        known(&store, &chat).await;
        run(&store, unarchive_says(true)).await;
        run(&store, setting(&chat, Field::Archived, 1, 1_000)).await;
        let after = |messages: Vec<IncomingMessage>| {
            let (store, chat) = (store.clone(), chat.clone());
            async move {
                store
                    .write("history", move |tx| after_history(tx, &chat, &messages))
                    .await
                    .expect("after history")
            }
        };
        assert!(
            after(vec![ring_at(&chat, "CALL0003", 2_000)])
                .await
                .is_empty(),
            "a call line of the push takes no archive back"
        );
        let newest = {
            let chat = chat.clone();
            store
                .read(move |conn| chat_state::newest_counted(conn, &chat))
                .await
                .expect("newest")
        };
        assert_eq!(newest, None, "and dates nothing");
        assert_eq!(mirror_archive(&store, &chat).await, Some((1, 1_000)));
    }
    // A text after the archive still makes it stale.
    {
        let (_dir, owner) = open();
        let store = owner.store();
        let chat = direct(FIRST);
        known(&store, &chat).await;
        run(&store, unarchive_says(true)).await;
        deliver(&store, vec![heard_at(&chat, "M1", 2_000)]).await;
        assert!(
            run(&store, setting(&chat, Field::Archived, 1, 1_000))
                .await
                .is_empty()
        );
    }
}
