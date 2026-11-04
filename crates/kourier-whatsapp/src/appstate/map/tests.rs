//! The settings map (W6a1, A3): each verb of D5's table, in upstream's own typed events (built with its `bon`
//! builders). Synthetic ids only (tails of 8 digits that are substrings of no other value).

use kourier_backend_api::BackendEvent;
use kourier_model::{ConvKey, ConversationPatch, MsgTarget, Patch, TargetAuthor, UserId};
use wa::sync_action_value as sav;
use wacore::chrono::DateTime;
use wacore::types::events::{
    ArchiveUpdate, ClearChatUpdate, Connected, ContactRemoved, ContactUpdate, DeleteChatUpdate,
    DeleteMessageForMeUpdate, Event, LockChatUpdate, MarkChatAsReadUpdate, MuteUpdate, PinUpdate,
    UnarchiveChatsSettingUpdate,
};
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

use super::{Mirror, Setting, core_event, map};
use crate::book::Change;
use crate::store::chat_state::Field;

const PEER: &str = "77700481537209";
const MEMBER: &str = "77700592640318";
const GROUP: &str = "120363900000481";

/// The action's own time (ms) and the dispatch time a mutation without one falls back to (ms).
const ACTION_MS: i64 = 1_760_000_111_000;
const DISPATCH_MS: i64 = 1_760_000_999_000;

fn at(ms: i64) -> wacore::chrono::DateTime<wacore::chrono::Utc> {
    DateTime::from_timestamp_millis(ms).expect("a time")
}

fn peer_jid() -> wacore_binary::Jid {
    format!("{PEER}@lid").parse().expect("jid")
}

fn peer() -> ConvKey {
    ConvKey::Direct(UserId::from_parts("lid", PEER).expect("a user"))
}

fn member() -> UserId {
    UserId::from_parts("lid", MEMBER).expect("a user")
}

fn group() -> ConvKey {
    ConvKey::Group(format!("wag:{GROUP}").into())
}

fn archive(jid: &str, archived: bool, action: Option<i64>) -> Event {
    let mut a = sav::ArchiveChatAction::default();
    a.archived = Some(archived);
    Event::ArchiveUpdate(
        ArchiveUpdate::builder()
            .jid(jid.parse().expect("jid"))
            .timestamp(at(DISPATCH_MS))
            .maybe_action_timestamp(action.map(at))
            .action(Box::new(a))
            .from_full_sync(false)
            .build(),
    )
}

fn pin(pinned: bool, action: Option<i64>) -> Event {
    let mut a = sav::PinAction::default();
    a.pinned = Some(pinned);
    Event::PinUpdate(
        PinUpdate::builder()
            .jid(peer_jid())
            .timestamp(at(DISPATCH_MS))
            .maybe_action_timestamp(action.map(at))
            .action(Box::new(a))
            .from_full_sync(false)
            .build(),
    )
}

fn mute(muted: Option<bool>, end: Option<i64>) -> Event {
    let mut a = sav::MuteAction::default();
    a.muted = muted;
    a.mute_end_timestamp = end;
    Event::MuteUpdate(
        MuteUpdate::builder()
            .jid(peer_jid())
            .timestamp(at(DISPATCH_MS))
            .action_timestamp(at(ACTION_MS))
            .action(Box::new(a))
            .from_full_sync(false)
            .build(),
    )
}

fn key(id: Option<&str>, from_me: bool, participant: Option<&str>) -> MessageField<wa::MessageKey> {
    let mut k = wa::MessageKey::default();
    k.id = id.map(str::to_owned);
    k.from_me = Some(from_me);
    k.participant = participant.map(str::to_owned);
    MessageField::some(k)
}

fn range(
    last: Option<i64>,
    last_system: Option<i64>,
    messages: Vec<(MessageField<wa::MessageKey>, i64)>,
) -> MessageField<sav::SyncActionMessageRange> {
    let mut r = sav::SyncActionMessageRange::default();
    r.last_message_timestamp = last;
    r.last_system_message_timestamp = last_system;
    r.messages = messages
        .into_iter()
        .map(|(key, time)| {
            let mut m = sav::SyncActionMessage::default();
            m.key = key;
            m.timestamp = Some(time);
            m
        })
        .collect();
    MessageField::some(r)
}

fn mark(read: Option<bool>, message_range: MessageField<sav::SyncActionMessageRange>) -> Event {
    let mut a = sav::MarkChatAsReadAction::default();
    a.read = read;
    a.message_range = message_range;
    Event::MarkChatAsReadUpdate(
        MarkChatAsReadUpdate::builder()
            .jid(peer_jid())
            .timestamp(at(DISPATCH_MS))
            .action_timestamp(at(ACTION_MS))
            .action(Box::new(a))
            .from_full_sync(false)
            .build(),
    )
}

fn setting(chat: ConvKey, field: Field, value: i64, action_ms: i64) -> Option<Mirror> {
    Some(Mirror::Chat(Setting {
        chat,
        field,
        value,
        action_ms,
    }))
}

fn target(chat: &ConvKey, author: TargetAuthor, local: &str) -> MsgTarget {
    MsgTarget {
        conv: Some(chat.clone()),
        author,
        local: local.into(),
        alternates: Vec::new(),
    }
}

#[test]
fn archive_pin_and_mute_become_settings_in_their_collections() {
    let jid = format!("{PEER}@lid");
    // Archive: the value, the collection upstream's schema table names, the action's own time before the dispatch time.
    let archived = map(&archive(&jid, true, Some(ACTION_MS))).expect("archive");
    assert_eq!(archived.collection, "regular_low");
    assert_eq!(
        archived.mirror,
        setting(peer(), Field::Archived, 1, ACTION_MS)
    );
    assert!(
        archived.events.is_empty(),
        "the core hears it from the mirror, not from the map"
    );
    let unarchived = map(&archive(&jid, false, None)).expect("unarchive");
    assert_eq!(
        unarchived.mirror,
        setting(peer(), Field::Archived, 0, DISPATCH_MS),
        "a mutation without a time of its own falls back to the dispatch time"
    );
    // A group is a conversation of its own.
    let in_group = map(&archive(&format!("{GROUP}@g.us"), true, Some(ACTION_MS))).expect("group");
    assert_eq!(
        in_group.mirror,
        setting(group(), Field::Archived, 1, ACTION_MS)
    );
    // Pin: a pin carries its time (the order); an unpin is 0.
    let pinned = map(&pin(true, Some(ACTION_MS))).expect("pin");
    assert_eq!(pinned.collection, "regular_low");
    assert_eq!(pinned.mirror, setting(peer(), Field::Pinned, 1, ACTION_MS));
    assert_eq!(
        map(&pin(false, Some(ACTION_MS))).expect("unpin").mirror,
        setting(peer(), Field::Pinned, 0, ACTION_MS)
    );
    // Mute: for ever (-1) is i64::MAX, a time stays as the phone wrote it (milliseconds), everything else is 0.
    let forever = map(&mute(Some(true), Some(-1))).expect("mute");
    assert_eq!(forever.collection, "regular_high");
    assert_eq!(
        forever.mirror,
        setting(peer(), Field::Muted, i64::MAX, ACTION_MS)
    );
    assert_eq!(
        map(&mute(Some(true), Some(1_760_086_400_000)))
            .expect("mute")
            .mirror,
        setting(peer(), Field::Muted, 1_760_086_400_000, ACTION_MS)
    );
    for (muted, end) in [
        (Some(false), Some(-1)),
        (Some(false), Some(1_760_086_400_000)),
        (Some(true), Some(0)),
        (Some(true), None),
        (None, None),
    ] {
        assert_eq!(
            map(&mute(muted, end)).expect("mute").mirror,
            setting(peer(), Field::Muted, 0, ACTION_MS),
            "{muted:?} {end:?}"
        );
    }
}

#[test]
fn a_chat_that_is_no_conversation_maps_to_nothing_and_so_do_the_verbs_kourier_ignores() {
    for jid in [
        "status@broadcast",
        "120363111222333@newsletter",
        "1234567890@broadcast",
    ] {
        assert_eq!(map(&archive(jid, true, Some(ACTION_MS))), None, "{jid}");
    }
    let lock = Event::LockChatUpdate(
        LockChatUpdate::builder()
            .jid(peer_jid())
            .timestamp(at(DISPATCH_MS))
            .action(Box::new(sav::LockChatAction::default()))
            .from_full_sync(false)
            .build(),
    );
    assert_eq!(map(&lock), None, "a locked chat is shown like any chat");
    assert_eq!(map(&Event::Connected(Connected::builder().build())), None);
    // The account's setting: the phone's flag as it is (true = a new message unarchives its chat), in regular_low.
    for flag in [true, false] {
        let mut a = sav::UnarchiveChatsSetting::default();
        a.unarchive_chats = Some(flag);
        let event = Event::UnarchiveChatsSettingUpdate(
            UnarchiveChatsSettingUpdate::builder()
                .unarchive_chats(flag)
                .timestamp(at(DISPATCH_MS))
                .action(Box::new(a))
                .from_full_sync(false)
                .build(),
        );
        let mapped = map(&event).expect("setting");
        assert_eq!(mapped.collection, "regular_low");
        assert_eq!(mapped.mirror, Some(Mirror::Unarchive(flag)));
        assert!(mapped.events.is_empty());
    }
}

#[test]
fn marked_unread_and_read_with_a_marker_up_to_the_newest_message_of_the_range() {
    // Marked unread: the mirror says 1, nothing else.
    let unread = map(&mark(Some(false), MessageField::none())).expect("unread");
    assert_eq!(unread.collection, "regular_low");
    assert_eq!(
        unread.mirror,
        setting(peer(), Field::MarkedUnread, 1, ACTION_MS)
    );
    assert!(unread.events.is_empty());
    // Read: the mirror says 0, and the core hears a read marker up to the message with the latest time (the list is
    // not in time order; an entry without a valid id is passed over).
    let messages = vec![
        (key(Some("3EB0AA01"), false, None), 1_760_000_050),
        (key(Some("3EB0AA03"), true, None), 1_760_000_090),
        (key(Some(""), false, None), 1_760_000_099),
        (key(Some("3EB0AA02"), false, None), 1_760_000_070),
    ];
    let read = map(&mark(
        Some(true),
        range(Some(1_760_000_090), None, messages),
    ))
    .expect("read");
    assert_eq!(
        read.mirror,
        setting(peer(), Field::MarkedUnread, 0, ACTION_MS)
    );
    assert_eq!(
        read.events,
        vec![BackendEvent::ReadMarker {
            conv: Some(peer()),
            up_to: target(&peer(), TargetAuthor::Own, "3EB0AA03"),
        }]
    );
    // A read with no message to name is no marker (read-by-time is D5's known limit), but still unmarks the chat.
    for none in [
        MessageField::none(),
        range(Some(1_760_000_090), None, vec![]),
    ] {
        let read = map(&mark(Some(true), none)).expect("read");
        assert_eq!(
            read.mirror,
            setting(peer(), Field::MarkedUnread, 0, ACTION_MS)
        );
        assert!(read.events.is_empty());
    }
    // A record that says neither is nothing.
    assert_eq!(map(&mark(None, MessageField::none())), None);
}

fn clear(range: MessageField<sav::SyncActionMessageRange>, jid: &str) -> Event {
    let mut a = sav::ClearChatAction::default();
    a.message_range = range;
    Event::ClearChatUpdate(
        ClearChatUpdate::builder()
            .jid(jid.parse().expect("jid"))
            .delete_starred(false)
            .delete_media(false)
            .timestamp(at(DISPATCH_MS))
            .action_timestamp(at(ACTION_MS))
            .action(Box::new(a))
            .from_full_sync(false)
            .build(),
    )
}

fn delete(range: MessageField<sav::SyncActionMessageRange>) -> Event {
    let mut a = sav::DeleteChatAction::default();
    a.message_range = range;
    Event::DeleteChatUpdate(
        DeleteChatUpdate::builder()
            .jid(peer_jid())
            .delete_media(false)
            .timestamp(at(DISPATCH_MS))
            .action_timestamp(at(ACTION_MS))
            .action(Box::new(a))
            .from_full_sync(false)
            .build(),
    )
}

#[test]
fn a_clear_and_a_delete_of_a_chat_claim_the_ranges_time_in_milliseconds_and_name_its_keys() {
    let messages = || {
        vec![
            (key(Some("3EB0BB01"), true, None), 1_760_000_050),
            (key(Some("3EB0BB02"), false, None), 1_760_000_070),
        ]
    };
    // The newer of the two times (seconds) times 1000: the system line is the newest here.
    let cleared = map(&clear(
        range(Some(1_760_000_070), Some(1_760_000_080), messages()),
        &format!("{PEER}@lid"),
    ))
    .expect("clear");
    assert_eq!(cleared.collection, "regular_high");
    assert_eq!(cleared.mirror, None, "a clear leaves nothing in the mirror");
    assert_eq!(
        cleared.events,
        vec![BackendEvent::ConversationCleared {
            conv: peer(),
            up_to: vec![
                target(&peer(), TargetAuthor::Own, "3EB0BB01"),
                target(
                    &peer(),
                    TargetAuthor::User(match peer() {
                        ConvKey::Direct(user) => user,
                        _ => unreachable!(),
                    }),
                    "3EB0BB02"
                ),
            ],
            remove: false,
            before_ms: Some(1_760_000_080_000),
            at: ACTION_MS,
        }]
    );
    // The delete is the same claim with `remove`; in a group the key's participant is the author.
    let deleted = map(&delete(range(Some(1_760_000_070), None, vec![]))).expect("delete");
    assert_eq!(deleted.collection, "regular_high");
    assert_eq!(
        deleted.events,
        vec![BackendEvent::ConversationCleared {
            conv: peer(),
            up_to: vec![],
            remove: true,
            before_ms: Some(1_760_000_070_000),
            at: ACTION_MS,
        }]
    );
    let in_group = map(&clear(
        range(
            None,
            None,
            vec![(
                key(Some("3EB0BB03"), false, Some(&format!("{MEMBER}@lid"))),
                1_760_000_071,
            )],
        ),
        &format!("{GROUP}@g.us"),
    ))
    .expect("group clear");
    assert_eq!(
        in_group.events,
        vec![BackendEvent::ConversationCleared {
            conv: group(),
            up_to: vec![target(&group(), TargetAuthor::User(member()), "3EB0BB03")],
            remove: false,
            before_ms: None,
            at: ACTION_MS,
        }]
    );
    // No bound at all (no range, or a range with no time and no key) says nothing the core could use, but the
    // collection was still mentioned.
    for none in [
        MessageField::none(),
        range(None, None, vec![]),
        range(Some(0), Some(-5), vec![]),
    ] {
        let said = map(&clear(none, &format!("{PEER}@lid"))).expect("clear");
        assert_eq!(said.collection, "regular_high");
        assert!(said.events.is_empty());
    }
}

fn forget(jid: &str, from_me: bool, participant: Option<&str>, id: &str) -> Event {
    let mut a = sav::DeleteMessageForMeAction::default();
    a.delete_media = Some(false);
    Event::DeleteMessageForMeUpdate(
        DeleteMessageForMeUpdate::builder()
            .chat_jid(jid.parse().expect("jid"))
            .maybe_participant_jid(participant.map(|p| p.parse().expect("jid")))
            .message_id(id.to_owned())
            .from_me(from_me)
            .timestamp(at(DISPATCH_MS))
            .action_timestamp(at(ACTION_MS))
            .action(Box::new(a))
            .from_full_sync(false)
            .build(),
    )
}

#[test]
fn a_delete_for_me_names_the_message_by_who_wrote_it() {
    let deleted = |target: MsgTarget| {
        vec![BackendEvent::Deleted {
            target,
            by: None,
            for_everyone: false,
            at: ACTION_MS,
        }]
    };
    let peer_id = format!("{PEER}@lid");
    let group_id = format!("{GROUP}@g.us");
    // Ours, in a chat with a person; theirs, in the same chat.
    let own = map(&forget(&peer_id, true, None, "3EB0CC01")).expect("own");
    assert_eq!(own.collection, "regular_high");
    assert_eq!(own.mirror, None);
    assert_eq!(
        own.events,
        deleted(target(&peer(), TargetAuthor::Own, "3EB0CC01"))
    );
    let ConvKey::Direct(person) = peer() else {
        unreachable!()
    };
    assert_eq!(
        map(&forget(&peer_id, false, None, "3EB0CC02"))
            .expect("theirs")
            .events,
        deleted(target(&peer(), TargetAuthor::User(person), "3EB0CC02"))
    );
    // In a group: a member by the participant, nobody we can name when the record has none, ours as ours.
    assert_eq!(
        map(&forget(
            &group_id,
            false,
            Some(&format!("{MEMBER}@lid")),
            "3EB0CC03"
        ))
        .expect("member")
        .events,
        deleted(target(&group(), TargetAuthor::User(member()), "3EB0CC03"))
    );
    assert_eq!(
        map(&forget(&group_id, false, None, "3EB0CC04"))
            .expect("unknown")
            .events,
        deleted(target(&group(), TargetAuthor::Unknown, "3EB0CC04"))
    );
    assert_eq!(
        map(&forget(&group_id, true, None, "3EB0CC05"))
            .expect("ours")
            .events,
        deleted(target(&group(), TargetAuthor::Own, "3EB0CC05"))
    );
    // An id the contract refuses is no event at all.
    assert_eq!(map(&forget(&peer_id, true, None, "")), None);
}

#[test]
fn the_cores_words_for_a_changed_value() {
    let patch = |f: &dyn Fn(&mut ConversationPatch)| {
        let mut patch = ConversationPatch::new(peer());
        f(&mut patch);
        Some(BackendEvent::ConversationUpsert(patch))
    };
    assert_eq!(
        core_event(&peer(), Field::Archived, 1),
        patch(&|p| p.archived = Some(true))
    );
    assert_eq!(
        core_event(&peer(), Field::Archived, 0),
        patch(&|p| p.archived = Some(false))
    );
    assert_eq!(
        core_event(&peer(), Field::MarkedUnread, 1),
        patch(&|p| p.marked_unread = Some(true))
    );
    assert_eq!(
        core_event(&peer(), Field::MarkedUnread, 0),
        patch(&|p| p.marked_unread = Some(false))
    );
    assert_eq!(
        core_event(&peer(), Field::Muted, i64::MAX),
        patch(&|p| p.muted_until = Patch::Set(i64::MAX))
    );
    assert_eq!(
        core_event(&peer(), Field::Muted, 1_760_086_400_000),
        patch(&|p| p.muted_until = Patch::Set(1_760_086_400_000))
    );
    assert_eq!(
        core_event(&peer(), Field::Muted, 0),
        patch(&|p| p.muted_until = Patch::Clear)
    );
    // A pin is the full list's business, built from the mirror.
    assert_eq!(core_event(&peer(), Field::Pinned, 1), None);
}

/// An archive action that names a message range.
fn archive_ranged(
    archived: bool,
    action: Option<i64>,
    message_range: MessageField<sav::SyncActionMessageRange>,
) -> Event {
    let mut a = sav::ArchiveChatAction::default();
    a.archived = Some(archived);
    a.message_range = message_range;
    Event::ArchiveUpdate(
        ArchiveUpdate::builder()
            .jid(peer_jid())
            .timestamp(at(DISPATCH_MS))
            .maybe_action_timestamp(action.map(at))
            .action(Box::new(a))
            .from_full_sync(true)
            .build(),
    )
}

/// F1 (INTEROP-1): an archive is dated by the last message of its range (WhatsApp writes seconds), not by when it was
/// written: a record the phone keeps after its own setting moved the chat out keeps naming the old range. No usable
/// range: the action's own time. An unarchive has no range worth reading: the action's time.
#[test]
fn an_archive_is_dated_by_the_last_message_of_its_range() {
    let last_s = 1_760_000_050;
    let dated = |event| match map(&event).expect("archive").mirror {
        Some(Mirror::Chat(setting)) => (setting.field, setting.value, setting.action_ms),
        other => panic!("not a chat setting: {other:?}"),
    };
    assert_eq!(
        dated(archive_ranged(
            true,
            Some(ACTION_MS),
            range(Some(last_s), None, Vec::new())
        )),
        (Field::Archived, 1, last_s * 1000),
        "seconds to milliseconds, the range wins over the action's time"
    );
    for (name, message_range) in [
        ("no range", MessageField::none()),
        ("a range with no time", range(None, None, Vec::new())),
        ("a range with a zero time", range(Some(0), None, Vec::new())),
    ] {
        assert_eq!(
            dated(archive_ranged(true, Some(ACTION_MS), message_range)),
            (Field::Archived, 1, ACTION_MS),
            "{name}: the action's time"
        );
    }
    assert_eq!(
        dated(archive_ranged(
            false,
            Some(ACTION_MS),
            range(Some(last_s), None, Vec::new())
        )),
        (Field::Archived, 0, ACTION_MS),
        "an unarchive is dated by the action"
    );
}

/// The same event as a record of a snapshot's replay: upstream's own word, set on every kind the map reads.
fn replayed(mut event: Event) -> Event {
    match &mut event {
        Event::ArchiveUpdate(u) => u.from_full_sync = true,
        Event::PinUpdate(u) => u.from_full_sync = true,
        Event::MuteUpdate(u) => u.from_full_sync = true,
        Event::MarkChatAsReadUpdate(u) => u.from_full_sync = true,
        Event::ClearChatUpdate(u) => u.from_full_sync = true,
        Event::DeleteChatUpdate(u) => u.from_full_sync = true,
        Event::DeleteMessageForMeUpdate(u) => u.from_full_sync = true,
        Event::UnarchiveChatsSettingUpdate(u) => u.from_full_sync = true,
        Event::ContactUpdate(u) => u.from_full_sync = true,
        Event::ContactRemoved(u) => u.from_full_sync = true,
        _ => panic!("an event this map reads"),
    }
    event
}

/// DUR-1: the map carries upstream's `from_full_sync` of every verb it reads, so the handler can tell a snapshot's
/// replay from a patch. Kills "the flag dropped" (always false) and "the flag taken for every verb" (always true).
#[test]
fn every_verb_carries_whether_it_came_from_a_snapshot() {
    let jid = format!("{PEER}@lid");
    let some_range = || range(Some(1_760_000_000), None, Vec::new());
    let mut a = sav::UnarchiveChatsSetting::default();
    a.unarchive_chats = Some(true);
    let setting = Event::UnarchiveChatsSettingUpdate(
        UnarchiveChatsSettingUpdate::builder()
            .unarchive_chats(true)
            .timestamp(at(DISPATCH_MS))
            .action(Box::new(a))
            .from_full_sync(false)
            .build(),
    );
    let patches = [
        archive(&jid, true, Some(ACTION_MS)),
        pin(true, Some(ACTION_MS)),
        mute(Some(true), Some(-1)),
        mark(Some(false), MessageField::none()),
        clear(some_range(), &jid),
        delete(some_range()),
        forget(&jid, false, None, "3EB0CC06"),
        setting,
        contact(&jid, Some("Ann Example")),
        removed(&jid),
    ];
    for patch in patches {
        let patched = map(&patch).expect("a mapped verb");
        assert!(!patched.from_full_sync, "a patch is no replay");
        let replay = map(&replayed(patch)).expect("a mapped verb");
        assert!(replay.from_full_sync, "a snapshot's record says so");
    }
}

/// The phone's `contact` mutation for the id `jid`: its full name, if any.
fn contact(jid: &str, name: Option<&str>) -> Event {
    let mut a = sav::ContactAction::default();
    a.full_name = name.map(str::to_owned);
    Event::ContactUpdate(
        ContactUpdate::builder()
            .jid(jid.parse().expect("jid"))
            .timestamp(at(DISPATCH_MS))
            .action(Box::new(a))
            .from_full_sync(false)
            .build(),
    )
}

/// The same contact's REMOVE.
fn removed(jid: &str) -> Event {
    Event::ContactRemoved(
        ContactRemoved::builder()
            .jid(jid.parse().expect("jid"))
            .timestamp(at(DISPATCH_MS))
            .from_full_sync(false)
            .build(),
    )
}

/// W6a2, B8b: a `contact` mutation is a row of the address book's mirror (the name is the full name; the verb lives in
/// `critical_unblock_low`), a REMOVE and a mutation that names nobody are its removal, and something that is no person is
/// nothing. Mutants: the REMOVE read as a put; the collection of another verb.
#[test]
fn a_contact_mutation_is_the_address_books_row_and_a_remove_is_its_removal() {
    let jid = format!("{PEER}@s.whatsapp.net");
    let put = map(&contact(&jid, Some("  Ann Example "))).expect("a mapped verb");
    assert_eq!(put.collection, "critical_unblock_low");
    assert!(
        put.events.is_empty(),
        "the zone makes the events, from the mirror"
    );
    let Some(Mirror::Book(Change::Put(entry))) = &put.mirror else {
        panic!("a row");
    };
    assert_eq!(entry.row().name, "Ann Example");
    assert_eq!(
        entry.row().contact,
        UserId::from_parts("pn", PEER).expect("a user").as_str()
    );

    for event in [removed(&jid), contact(&jid, None)] {
        let gone = map(&event).expect("a mapped verb");
        assert_eq!(gone.collection, "critical_unblock_low");
        assert!(
            matches!(&gone.mirror, Some(Mirror::Book(Change::Remove(_)))),
            "a removal"
        );
    }
    assert!(
        map(&removed("status@broadcast")).is_none(),
        "no person, no row"
    );
    assert!(map(&contact("status@broadcast", Some("x"))).is_none());
}
