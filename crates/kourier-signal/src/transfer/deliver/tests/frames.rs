//! Step S4a: what a chat frame carries, delivered. The chat's upsert takes archived, marked
//! unread and the mute; the 1:1 timer and its version are in `peers` before the features are
//! computed (so the features event carries them, also in a redo); the pins come as ONE list
//! in a row of their own after every chat's upsert, sorted by the phone's order; a pinned
//! chat is shown without a loud row. A real `backend.db` with an account behind it.

use kourier_db::OutboxSource as _;

use super::review::{aci, fed_from, outbox, reading};
use super::*;
use crate::actor::testing;
use crate::receive::tests::fixture;
use crate::transfer::stage::ChatFrame;

/// One chat of [`stage_of`]: its rows (all loud when `loud`), its pin, its frame.
#[derive(Clone, Copy, Default)]
struct Spec {
    rows: u64,
    loud: bool,
    pinned: Option<u32>,
    frame: ChatFrame,
}

/// A stage of people with an ACI, one chat each (chat `i` the more recent the higher `i`).
fn stage_of(dir: &std::path::Path, chats: &[Spec]) -> Recorded {
    let mut writer = StageWriter::create(dir, &key(), StageLimits::default()).unwrap();
    for (i, spec) in chats.iter().enumerate() {
        let i = i as u32;
        let contact = StagedContact {
            user: peer(i),
            aci: Some(aci(i)),
            pni: None,
            e164: None,
            profile_key: None,
            profile_sharing: true,
            blocked: false,
            hidden: false,
            unregistered_at: None,
            profile_name: None,
            system_name: None,
            nickname: None,
        };
        writer.contact(i, &contact).unwrap();
        let mut chat = StagedChat::direct(i);
        chat.frame = spec.frame;
        writer
            .chat(i, &ConvKey::Direct(peer(i)), &chat, spec.pinned)
            .unwrap();
    }
    let mut n = 0;
    for (i, spec) in chats.iter().enumerate() {
        for _ in 0..spec.rows {
            n += 1;
            writer.item(i as u32, n, &message(i as u32, n, 10)).unwrap();
        }
    }
    let counters: Vec<ChatCounters> = chats
        .iter()
        .enumerate()
        .map(|(i, spec)| ChatCounters {
            idx: i as u32,
            n_rows: spec.rows,
            n_loud: if spec.loud { spec.rows } else { 0 },
            last_at: 1_000 * (i as i64 + 1),
        })
        .collect();
    let totals = writer.commit(&counters, n + 7).unwrap();
    Recorded {
        messages: totals.messages,
        chats: totals.chats,
        frames: n + 7,
    }
}

/// Appends every chunk up to the first row of messages; returns the kinds appended.
fn deliver_up_to_the_messages<T: SignalTransport>(
    shared: &Shared<T>,
    dir: &std::path::Path,
    recorded: Recorded,
) -> Vec<i64> {
    let mut kinds = Vec::new();
    for chunk in fed_from(&shared.db, dir, recorded) {
        if matches!(chunk.payload, Payload::Messages { .. }) {
            break;
        }
        kinds.push(chunk.kind);
        append(shared, chunk, NOW).unwrap();
    }
    kinds
}

/// Every event of `core_outbox`, in order.
pub(super) fn events(db: &ZoneDb) -> Vec<BackendEvent> {
    outbox(db).into_iter().flat_map(|(_, e)| e).collect()
}

/// The upserts of `conv` merged field by field (the later wins), as JSON.
pub(super) fn upsert_of(
    events: &[BackendEvent],
    conv: &ConvKey,
) -> serde_json::Map<String, serde_json::Value> {
    let mut merged = serde_json::Map::new();
    for event in events {
        if let BackendEvent::ConversationUpsert(patch) = event
            && &patch.key == conv
            && let serde_json::Value::Object(fields) = serde_json::to_value(patch).unwrap()
        {
            merged.extend(fields);
        }
    }
    merged
}

fn frame(timer_secs: Option<u32>, timer_version: u32) -> ChatFrame {
    ChatFrame {
        timer_secs,
        timer_version,
        ..ChatFrame::default()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_chats_row_carries_archive_marks_and_mute_and_the_timer_rides_on_the_features() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage_of(
        dir.path(),
        &[
            Spec {
                rows: 2,
                loud: true,
                frame: ChatFrame {
                    archived: true,
                    marked_unread: true,
                    muted_until: Some(i64::MAX),
                    timer_secs: Some(3_600),
                    timer_version: 4,
                },
                ..Spec::default()
            },
            Spec {
                rows: 1,
                loud: true,
                // A mute that ran out on the phone stays a time in the past.
                frame: ChatFrame {
                    muted_until: Some(5_000),
                    ..frame(None, 1)
                },
                ..Spec::default()
            },
            // The phone writes the version also when the timer is off.
            Spec {
                rows: 1,
                loud: true,
                frame: frame(None, 7),
                ..Spec::default()
            },
        ],
    );
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;
    deliver_up_to_the_messages(&shared, dir.path(), recorded);
    let all = events(&shared.db);

    let first = upsert_of(&all, &ConvKey::Direct(peer(0)));
    assert_eq!(first["archived"], true);
    assert_eq!(first["marked_unread"], true);
    assert_eq!(first["muted_until"]["Set"], i64::MAX, "muted for ever");
    assert_eq!(
        first["timer"]["Set"]["secs"], 3_600,
        "the timer, in seconds"
    );
    assert_eq!(first["timer_version"], 4);
    assert!(first.contains_key("features"));
    // The timer is in the FEATURES event, the one in front of the chat's own upsert.
    let features = all
        .iter()
        .find_map(|event| match event {
            BackendEvent::ConversationUpsert(patch)
                if patch.key == ConvKey::Direct(peer(0)) && patch.features.is_some() =>
            {
                Some(patch)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(features.timer_version, Some(4));
    let sid = |i: u32| ServiceId::from(Aci::from(aci(i))).service_id_string();
    let row = shared.db.read(|c| peers::load(c, &sid(0))).unwrap();
    assert_eq!((row.expire_timer, row.expire_timer_version), (3_600, 4));

    let second = upsert_of(&all, &ConvKey::Direct(peer(1)));
    assert_eq!(second["muted_until"]["Set"], 5_000, "a mute in the past");
    assert!(!second.contains_key("archived") && !second.contains_key("marked_unread"));
    assert!(!second.contains_key("timer"));

    let third = upsert_of(&all, &ConvKey::Direct(peer(2)));
    assert!(!third.contains_key("timer") && !third.contains_key("muted_until"));
    let row = shared.db.read(|c| peers::load(c, &sid(2))).unwrap();
    assert_eq!(
        (row.expire_timer, row.expire_timer_version),
        (0, 7),
        "the version is stored without a timer"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_pins_come_after_every_upsert_in_one_row_sorted_by_the_phones_order() {
    let dir = tempfile::tempdir().unwrap();
    // Orders with gaps, not in the order of the chats; one chat is not pinned; the pinned
    // chat 3 has no loud row and chat 4 no row at all: both are shown all the same.
    let recorded = stage_of(
        dir.path(),
        &[
            Spec {
                rows: 1,
                loud: true,
                pinned: Some(30),
                ..Spec::default()
            },
            Spec {
                rows: 1,
                loud: true,
                ..Spec::default()
            },
            Spec {
                rows: 1,
                loud: true,
                pinned: Some(2),
                ..Spec::default()
            },
            Spec {
                rows: 1,
                loud: false,
                pinned: Some(7),
                ..Spec::default()
            },
            Spec {
                rows: 0,
                loud: false,
                pinned: Some(9),
                ..Spec::default()
            },
            // Quiet and not pinned: not shown.
            Spec {
                rows: 1,
                loud: false,
                ..Spec::default()
            },
        ],
    );
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;
    let kinds = deliver_up_to_the_messages(&shared, dir.path(), recorded);
    assert_eq!(
        kinds.iter().filter(|kind| **kind == KIND_PINS).count(),
        1,
        "one chunk of pins"
    );
    assert_eq!(kinds.last(), Some(&KIND_PINS), "after every chunk of chats");
    let rows = outbox(&shared.db);
    let last = &rows.last().unwrap().1;
    assert_eq!(
        last,
        &vec![BackendEvent::PinnedConversations(vec![
            ConvKey::Direct(peer(2)),
            ConvKey::Direct(peer(3)),
            ConvKey::Direct(peer(4)),
            ConvKey::Direct(peer(0)),
        ])],
        "a row of its own, ascending pinned order"
    );
    let upserted: Vec<ConvKey> = rows[..rows.len() - 1]
        .iter()
        .flat_map(|(_, events)| events)
        .filter_map(|event| match event {
            BackendEvent::ConversationUpsert(patch) => Some(patch.key.clone()),
            _ => None,
        })
        .collect();
    for i in [0, 1, 2, 3, 4] {
        assert!(
            upserted.contains(&ConvKey::Direct(peer(i))),
            "chat {i} is upserted before the pins"
        );
    }
    assert!(
        !upserted.contains(&ConvKey::Direct(peer(5))),
        "a quiet chat that is not pinned is not shown"
    );
    // The chat without a row still hears that nothing follows.
    let messages: Vec<Chunk> = fed_from(&shared.db, dir.path(), recorded);
    assert!(messages.iter().any(|chunk| matches!(
        &chunk.payload,
        Payload::Messages { conv, messages, has_more: false, .. }
            if *conv == ConvKey::Direct(peer(4)) && messages.is_empty()
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn a_redo_of_a_chats_row_states_the_timer_again() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage_of(
        dir.path(),
        &[
            Spec {
                rows: 1,
                loud: true,
                frame: frame(Some(60), 3),
                ..Spec::default()
            },
            Spec {
                rows: 1,
                loud: true,
                frame: frame(Some(120), 2),
                ..Spec::default()
            },
        ],
    );
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;
    let mut chats_seq = None;
    for chunk in fed_from(&shared.db, dir.path(), recorded) {
        if matches!(chunk.payload, Payload::Messages { .. }) {
            break;
        }
        let is_chats = matches!(chunk.payload, Payload::Chats(_));
        let done = append(&shared, chunk, NOW).unwrap();
        if is_chats {
            chats_seq = done.seq;
        }
    }
    let chats_seq = chats_seq.unwrap();
    shared
        .db
        .quarantine(chats_seq, "refused by a test")
        .unwrap();
    assert!(settle(&shared.db, MAX_REFUSED_RUN).unwrap().new_pieces);
    for chunk in fed_from(&shared.db, dir.path(), recorded) {
        if matches!(chunk.place, Place::Redo { .. }) {
            append(&shared, chunk, NOW).unwrap();
        }
    }
    let again: Vec<BackendEvent> = outbox(&shared.db)
        .into_iter()
        .filter(|(seq, _)| *seq > chats_seq)
        .flat_map(|(_, events)| events)
        .collect();
    for (i, secs, version) in [(0, 60, 3), (1, 120, 2)] {
        let fields = upsert_of(&again, &ConvKey::Direct(peer(i)));
        assert_eq!(fields["timer"]["Set"]["secs"], secs, "chat {i}");
        assert_eq!(fields["timer_version"], version, "chat {i}");
    }
}

/// Finding 2 of the durability review: a chats zone that runs again writes the archive's
/// pair of timer and version only when the version is not below the stored one. The user
/// changes the timers in Kourier (a version above the archive's) while the chats row waits;
/// the row is refused and its chats are appended again as singles.
#[tokio::test(flavor = "current_thread")]
async fn a_redo_after_the_users_timer_change_keeps_the_users_timer_and_version() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage_of(
        dir.path(),
        &[
            Spec {
                rows: 1,
                loud: true,
                frame: frame(None, 3),
                ..Spec::default()
            },
            Spec {
                rows: 1,
                loud: true,
                frame: frame(Some(60), 3),
                ..Spec::default()
            },
        ],
    );
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;
    let mut chats_seq = None;
    for chunk in fed_from(&shared.db, dir.path(), recorded) {
        if matches!(chunk.payload, Payload::Messages { .. }) {
            break;
        }
        let is_chats = matches!(chunk.payload, Payload::Chats(_));
        let done = append(&shared, chunk, NOW).unwrap();
        if is_chats {
            chats_seq = done.seq;
        }
    }
    let chats_seq = chats_seq.unwrap();
    let sid = |i: u32| ServiceId::from(Aci::from(aci(i))).service_id_string();
    // The user changes both timers while the row waits (`timer::change` stores the version
    // one above ours): one hour in chat 0, off in chat 1.
    shared
        .db
        .zone(|tx| -> Result<()> {
            peers::set_expire_timer(tx, &sid(0), 3_600, 4, NOW)?;
            peers::set_expire_timer(tx, &sid(1), 0, 4, NOW)?;
            Ok(())
        })
        .unwrap();
    shared
        .db
        .quarantine(chats_seq, "refused by a test")
        .unwrap();
    assert!(settle(&shared.db, MAX_REFUSED_RUN).unwrap().new_pieces);
    for chunk in fed_from(&shared.db, dir.path(), recorded) {
        if matches!(chunk.place, Place::Redo { .. }) {
            append(&shared, chunk, NOW).unwrap();
        }
    }
    let pair = |i: u32| {
        let row = shared.db.read(|c| peers::load(c, &sid(i))).unwrap();
        (row.expire_timer, row.expire_timer_version)
    };
    assert_eq!(pair(0), (3_600, 4), "the user's hour stands");
    assert_eq!(pair(1), (0, 4), "the user's 'off' stands");
    // What the core is told again is the user's pair too, not the archive's.
    let again: Vec<BackendEvent> = outbox(&shared.db)
        .into_iter()
        .filter(|(seq, _)| *seq > chats_seq)
        .flat_map(|(_, events)| events)
        .collect();
    let first = upsert_of(&again, &ConvKey::Direct(peer(0)));
    assert_eq!(first["timer"]["Set"]["secs"], 3_600);
    assert_eq!(first["timer_version"], 4);
    let second = upsert_of(&again, &ConvKey::Direct(peer(1)));
    assert!(
        !second.contains_key("timer"),
        "the archive's minute does not come back: {second:?}"
    );
}

/// Appends every chunk up to the first row of messages or the pins, whichever comes first;
/// returns the seq of the chats row.
pub(super) fn deliver_the_chats<T: SignalTransport>(
    shared: &Shared<T>,
    dir: &std::path::Path,
    recorded: Recorded,
) -> i64 {
    let mut chats_seq = None;
    for chunk in fed_from(&shared.db, dir, recorded) {
        if matches!(chunk.payload, Payload::Messages { .. } | Payload::Pins(_)) {
            break;
        }
        let is_chats = matches!(chunk.payload, Payload::Chats(_));
        let done = append(shared, chunk, NOW).unwrap();
        if is_chats {
            chats_seq = done.seq;
        }
    }
    chats_seq.unwrap()
}

/// The pins chunk a feeder that starts from the stored state hands over, appended.
pub(super) fn append_the_pins<T: SignalTransport>(
    shared: &Shared<T>,
    dir: &std::path::Path,
    recorded: Recorded,
) -> Vec<ConvKey> {
    let before = outbox(&shared.db).len();
    let pins = fed_from(&shared.db, dir, recorded)
        .into_iter()
        .find(|chunk| chunk.kind == KIND_PINS)
        .expect("the pins chunk");
    append(shared, pins, NOW).unwrap();
    outbox(&shared.db)
        .into_iter()
        .skip(before)
        .flat_map(|(_, events)| events)
        .find_map(|event| match event {
            BackendEvent::PinnedConversations(keys) => Some(keys),
            _ => None,
        })
        .unwrap_or_default()
}

fn has_features(db: &ZoneDb, i: u32) -> bool {
    let sid = ServiceId::from(Aci::from(aci(i))).service_id_string();
    db.read(|c| peers::load(c, &sid))
        .unwrap()
        .features_hash
        .is_some()
}

/// Three pinned chats, one row each: the chats row holds all three.
fn three_pinned() -> Vec<Spec> {
    (1..=3)
        .map(|order| Spec {
            rows: 1,
            loud: true,
            pinned: Some(order),
            ..Spec::default()
        })
        .collect()
}

/// Finding 5 and 6 of the reviews: a chat whose single upsert was dropped for good is not
/// named in the pins (the core would make a bare chat of it), and the features of exactly
/// that chat are forgotten, not the others'.
#[tokio::test(flavor = "current_thread")]
async fn a_chat_whose_upsert_was_dropped_is_not_pinned_and_forgets_only_its_own_features() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage_of(dir.path(), &three_pinned());
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;
    let chats_seq = deliver_the_chats(&shared, dir.path(), recorded);
    assert!(
        (0..3).all(|i| has_features(&shared.db, i)),
        "all three sent"
    );
    // The core refuses the whole row: it becomes three singles, and the features of every
    // chat of the row are forgotten (the core never took them).
    shared
        .db
        .quarantine(chats_seq, "refused by a test")
        .unwrap();
    assert!(settle(&shared.db, MAX_REFUSED_RUN).unwrap().new_pieces);
    assert!((0..3).all(|i| !has_features(&shared.db, i)));
    // The singles are appended again, each with its features in front.
    let mut singles = Vec::new();
    for chunk in fed_from(&shared.db, dir.path(), recorded) {
        if matches!(chunk.place, Place::Redo { .. }) {
            singles.push(append(&shared, chunk, NOW).unwrap().seq.unwrap());
        }
    }
    assert_eq!(singles.len(), 3);
    assert!((0..3).all(|i| has_features(&shared.db, i)));
    // The core refuses one single for good: that chat is dropped.
    shared.db.quarantine(singles[1], "refused again").unwrap();
    let found = settle(&shared.db, MAX_REFUSED_RUN).unwrap();
    assert!(!found.new_pieces, "a single is dropped, not split");
    let dropped_chat = (0..3u32)
        .filter(|i| !has_features(&shared.db, *i))
        .collect::<Vec<_>>();
    assert_eq!(
        dropped_chat.len(),
        1,
        "exactly one chat forgot its features"
    );
    let gone = ConvKey::Direct(peer(dropped_chat[0]));
    let named = append_the_pins(&shared, dir.path(), recorded);
    assert_eq!(named.len(), 2, "{named:?}");
    assert!(!named.contains(&gone), "the dropped chat is not pinned");
}

/// Finding 6: a lost stage (`forget`) discards a chats piece that was waiting to be appended
/// again; the features of the chats it named were forgotten when the row was taken back, and
/// the features of a chat the row did not name stand.
#[tokio::test(flavor = "current_thread")]
async fn a_lost_stage_leaves_no_features_flag_of_a_chats_piece_it_discards() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage_of(dir.path(), &three_pinned());
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;
    // A person who has nothing to do with the row, whose chat the core has.
    let other = ServiceId::from(Aci::from(aci(99))).service_id_string();
    shared
        .db
        .zone(|tx| peers::set_features_hash(tx, &other, 77, NOW))
        .unwrap();
    let chats_seq = deliver_the_chats(&shared, dir.path(), recorded);
    shared
        .db
        .quarantine(chats_seq, "refused by a test")
        .unwrap();
    assert!(settle(&shared.db, MAX_REFUSED_RUN).unwrap().new_pieces);
    // The stage is lost before the piece was appended again.
    shared.db.zone(|tx| forget(tx, NOW)).unwrap();
    assert!(
        (0..3).all(|i| !has_features(&shared.db, i)),
        "the row's chats"
    );
    let kept = shared.db.read(|c| peers::load(c, &other)).unwrap();
    assert_eq!(kept.features_hash, Some(77), "nobody else's");
}

/// `append`: a row of chats that fails the check of the whole row (as the core would run it)
/// is not appended, and the features its zone recorded as sent are forgotten, those of the
/// chats it named and nobody else's: the core never gets them. The row cannot fail by data
/// (see `FAIL_THE_NEXT_WHOLE_ROW`), so the check is made to fail once.
#[tokio::test(flavor = "current_thread")]
async fn a_chats_row_that_fails_the_whole_row_check_forgets_the_features_it_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage_of(dir.path(), &three_pinned());
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;
    // A person who has nothing to do with the row, whose chat the core has.
    let other = ServiceId::from(Aci::from(aci(99))).service_id_string();
    shared
        .db
        .zone(|tx| peers::set_features_hash(tx, &other, 77, NOW))
        .unwrap();
    let mut dropped_row = None;
    for chunk in fed_from(&shared.db, dir.path(), recorded) {
        if matches!(chunk.payload, Payload::Messages { .. } | Payload::Pins(_)) {
            break;
        }
        let is_chats = matches!(chunk.payload, Payload::Chats(_));
        if is_chats {
            FAIL_THE_NEXT_WHOLE_ROW.set(true);
        }
        let done = append(&shared, chunk, NOW).unwrap();
        if is_chats {
            dropped_row = Some(done);
        }
    }
    let done = dropped_row.expect("the chats row");
    assert!(!FAIL_THE_NEXT_WHOLE_ROW.get(), "the switch was used");
    assert_eq!(done.seq, None, "nothing was appended for the row");
    assert_eq!(done.dropped, 3, "its three chats are counted dropped");
    assert!(
        outbox(&shared.db)
            .iter()
            .flat_map(|(_, events)| events)
            .all(|event| !matches!(event, BackendEvent::ConversationUpsert(_))),
        "no chat of the row reached core_outbox"
    );
    assert!(
        (0..3).all(|i| !has_features(&shared.db, i)),
        "the features of the dropped row's chats are forgotten"
    );
    let kept = shared.db.read(|c| peers::load(c, &other)).unwrap();
    assert_eq!(kept.features_hash, Some(77), "nobody else's");
}
