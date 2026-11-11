//! The intents of the write engine (A10b chunk B3): the gate, the filing in the order received,
//! the couplings, the mirrors, the automatic clear, the identity intent, dropping and the
//! overtaking by a message (critic X1). Synthetic ids only.

use std::rc::Rc;

use kourier_backend_api::{BackendEvent, decode_outbox_row};
use kourier_model::{CmdId, ConvKey, ConversationChange, ConversationPatch, ErrorKind, Patch};
use libsignal_core::{Aci, ServiceId};
use tokio::sync::Notify;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::*;
use crate::proto::manifest_record::identifier::Type as ItemType;
use crate::receive::tests::{Fixture, fixture};
use crate::service_id::aci_user_id;
use crate::storage::{Decrypted, merge_zone};

// ---------------------------------------------------------------------------------------------
// The world
// ---------------------------------------------------------------------------------------------

fn aci(n: u128) -> Aci {
    Aci::from(Uuid::from_u128(n))
}

/// The 1:1 chat with the person `n`.
fn chat(n: u128) -> ConvKey {
    ConvKey::Direct(aci_user_id(aci(n)).unwrap())
}

/// The record target of the person `n`.
fn person(n: u128) -> String {
    contact_target(&ServiceId::from(aci(n)).service_id_string())
}

/// The group with master key `[m; 32]`.
fn group_chat(m: u8) -> ConvKey {
    groups::GroupKeys::from_master([m; 32]).conv_key()
}

fn group_t(m: u8) -> String {
    group_target(&[m; 32])
}

fn item(key: u8, kind: ItemType, record: Record) -> Decrypted {
    let record = proto::StorageRecord {
        record: Some(record),
    };
    Decrypted {
        key: vec![key; 16],
        kind: kind as i32,
        raw: Zeroizing::new(record.encode_to_vec()),
        record,
    }
}

fn contact(key: u8, n: u128, edit: impl FnOnce(&mut proto::ContactRecord)) -> Decrypted {
    let mut record = proto::ContactRecord {
        aci_binary: Uuid::from_u128(n).as_bytes().to_vec(),
        ..Default::default()
    };
    edit(&mut record);
    item(key, ItemType::Contact, Record::Contact(record))
}

fn group_record(key: u8, m: u8, edit: impl FnOnce(&mut proto::GroupV2Record)) -> Decrypted {
    let mut record = proto::GroupV2Record {
        master_key: vec![m; 32],
        ..Default::default()
    };
    edit(&mut record);
    item(key, ItemType::Groupv2, Record::GroupV2(record))
}

fn account_record(key: u8, edit: impl FnOnce(&mut proto::AccountRecord)) -> Decrypted {
    let mut record = proto::AccountRecord::default();
    edit(&mut record);
    item(key, ItemType::Account, Record::Account(record))
}

fn contact_pin(n: u128) -> PinnedConversation {
    PinnedConversation::decode(&wire::contact_pin_entry(aci(n))[..]).unwrap()
}

fn group_pin(m: u8) -> PinnedConversation {
    PinnedConversation::decode(&wire::group_pin_entry(&[m; 32])[..]).unwrap()
}

struct World {
    f: Fixture,
    wake: Rc<Notify>,
    /// The last new record id `plan_and_row` handed out (`[n; 16]`).
    last_id: std::cell::Cell<u8>,
}

impl World {
    /// A linked account with the switch OFF (the default of every account).
    fn new() -> Self {
        Self {
            f: fixture(),
            wake: Rc::new(Notify::new()),
            last_id: std::cell::Cell::new(0xa0),
        }
    }

    /// A world with the switch on.
    fn on() -> Self {
        let w = Self::new();
        w.switch(true);
        w
    }

    fn switch(&self, on: bool) {
        self.f
            .db
            .zone(|tx| -> Result<()> {
                tx.execute("UPDATE account SET roaming_writes = ?1 WHERE id = 1", [on])?;
                Ok(())
            })
            .unwrap();
    }

    /// Merges `items` as the records of a sync at `now`, and forgets what that told the core.
    fn put(&self, now: i64, items: Vec<Decrypted>) {
        self.merge(now, items);
    }

    /// Merges `items` as the records of a sync at `now` and returns what that told the core.
    fn merge(&self, now: i64, items: Vec<Decrypted>) -> Vec<BackendEvent> {
        self.f
            .db
            .zone(|tx| merge_zone(tx, &self.f.account, &items, None, false, now, &self.wake))
            .unwrap();
        self.events()
    }

    /// The core has the group `[m; 32]` (its features were sent): a record of it is diffed and
    /// stated.
    fn core_has_group(&self, m: u8) {
        self.f
            .db
            .zone(|tx| -> Result<()> {
                tx.execute(
                    "UPDATE groups SET features_hash = 42 WHERE master_key = ?1",
                    [&[m; 32][..]],
                )?;
                Ok(())
            })
            .unwrap();
    }

    /// The group's mirror columns: archived, marked unread, mute end.
    fn group_mirror(&self, m: u8) -> (bool, bool, Option<i64>) {
        let id = groups::id_of_conv(&group_chat(m)).unwrap();
        let row = self
            .f
            .db
            .read(|c| groups::load_by_id(c, &id))
            .unwrap()
            .unwrap();
        (row.archived, row.marked_unread, row.muted_until)
    }

    /// What the core was last told of Note to Self's archive state.
    fn told_own(&self) -> Option<Roaming> {
        let sid = ServiceId::from(self.f.account.aci).service_id_string();
        self.f.db.read(|c| peers::load(c, &sid)).unwrap().roaming
    }

    /// Takes the events the core has not seen yet out of `core_outbox`.
    fn events(&self) -> Vec<BackendEvent> {
        let events = self
            .f
            .db
            .read(|c| kourier_db::outbox::peek(c, 10_000))
            .unwrap()
            .iter()
            .flat_map(|row| decode_outbox_row(&row.envelope).unwrap())
            .collect();
        self.f
            .db
            .zone(|tx| -> Result<()> {
                tx.execute("DELETE FROM core_outbox", [])?;
                Ok(())
            })
            .unwrap();
        events
    }

    fn click(
        &self,
        change: ConversationChange,
        conv: &ConvKey,
        now: i64,
    ) -> std::result::Result<(), Refused> {
        self.f
            .db
            .zone(|tx| intend(tx, &self.f.account, &change, conv, now, &self.wake))
            .unwrap()
    }

    fn archive(
        &self,
        conv: &ConvKey,
        archived: bool,
        now: i64,
    ) -> std::result::Result<(), Refused> {
        self.click(ConversationChange::SetArchived { archived }, conv, now)
    }

    fn mark(
        &self,
        conv: &ConvKey,
        marked_unread: bool,
        now: i64,
    ) -> std::result::Result<(), Refused> {
        self.click(
            ConversationChange::SetMarkedUnread { marked_unread },
            conv,
            now,
        )
    }

    fn pin(&self, conv: &ConvKey, pinned: bool, now: i64) -> std::result::Result<(), Refused> {
        self.click(ConversationChange::SetPinned { pinned }, conv, now)
    }

    fn mute(
        &self,
        conv: &ConvKey,
        until: Option<i64>,
        now: i64,
    ) -> std::result::Result<(), Refused> {
        self.click(ConversationChange::SetMuted { until }, conv, now)
    }

    fn automatic(&self, conv: &ConvKey, asked_at: i64, now: i64) -> Automatic {
        self.f
            .db
            .zone(|tx| {
                intend_automatic(
                    tx,
                    &self.f.account,
                    &ConversationChange::SetMarkedUnread {
                        marked_unread: false,
                    },
                    conv,
                    asked_at,
                    now,
                    &self.wake,
                )
            })
            .unwrap()
    }

    fn durable(&self, cmd: i64, conv: &ConvKey, asked_at: i64, now: i64) -> bool {
        self.f
            .db
            .zone(|tx| {
                file_durable(
                    tx,
                    &self.f.account,
                    CmdId(cmd),
                    conv,
                    &ConversationChange::SetMarkedUnread {
                        marked_unread: false,
                    },
                    asked_at,
                    now,
                    &self.wake,
                )
            })
            .unwrap()
    }

    fn intents(&self) -> Vec<Intent> {
        self.f.db.read(all_intents).unwrap()
    }

    fn intent(&self, target: &str, field: &str) -> Option<Intent> {
        self.f.db.read(|c| load_intent(c, target, field)).unwrap()
    }

    fn seq(&self) -> i64 {
        self.f
            .db
            .read(|c| -> Result<i64> {
                Ok(c.query_row(
                    "SELECT storage_intent_seq FROM account WHERE id = 1",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap()
    }

    fn jobs(&self) -> i64 {
        self.f
            .db
            .read(|c| jobs::count(c, JobKind::StorageWrite))
            .unwrap()
    }

    fn job_at(&self) -> Option<i64> {
        self.f
            .db
            .read(|c| -> Result<Option<i64>> {
                Ok(c.query_row(
                    "SELECT next_at FROM jobs WHERE kind = 'storage_write'",
                    [],
                    |r| r.get(0),
                )
                .optional()?)
            })
            .unwrap()
    }

    /// A write in flight that carries `planned` (a JSON list).
    fn in_flight(&self, planned: &str) {
        self.f
            .db
            .zone(|tx| -> Result<()> {
                tx.execute(
                    "INSERT INTO storage_write_inflight(id, version, new_keys, planned, generation,
                       planned_at) VALUES (1, 8, X'', ?1, 0, 100)",
                    [planned],
                )?;
                Ok(())
            })
            .unwrap();
    }

    fn in_flight_rows(&self) -> i64 {
        self.f
            .db
            .read(|c| -> Result<i64> {
                Ok(
                    c.query_row("SELECT count(*) FROM storage_write_inflight", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap()
    }

    /// What the core was last told of the 1:1 chat with the person `n`.
    fn told(&self, n: u128) -> Option<Roaming> {
        self.f
            .db
            .read(|c| peers::load(c, &ServiceId::from(aci(n)).service_id_string()))
            .unwrap()
            .roaming
    }

    fn set_pin_limit(&self, limit: &str) {
        self.f
            .db
            .zone(|tx| -> Result<()> {
                tx.execute(
                    "INSERT OR REPLACE INTO remote_config(name, value) VALUES (?1, ?2)",
                    (PIN_LIMIT_CONFIG, limit),
                )?;
                Ok(())
            })
            .unwrap();
    }
}

/// The first upsert of `conv` that states flags (a merge puts the features of a chat the core has
/// not got yet in front of its flags: that upsert states none and is skipped).
fn patch_of(events: &[BackendEvent], conv: &ConvKey) -> Option<ConversationPatch> {
    events.iter().find_map(|e| match e {
        BackendEvent::ConversationUpsert(p) if &p.key == conv && p.features.is_none() => {
            Some(p.clone())
        }
        _ => None,
    })
}

fn pin_lists(events: &[BackendEvent]) -> Vec<Vec<ConvKey>> {
    events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::PinnedConversations(list) => Some(list.clone()),
            _ => None,
        })
        .collect()
}

fn acks(events: &[BackendEvent]) -> Vec<CmdId> {
    events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::CommandPersisted { cmd } => Some(*cmd),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------------------------

/// §5.1 "gate", B1: with the switch off every one of the four clicks is refused and NOTHING is
/// stored: no intent, no seq taken, no event, no job. The same click is taken once the switch is on.
#[test]
fn with_the_switch_off_a_click_is_refused_and_nothing_is_stored() {
    let w = World::new();
    w.put(
        10,
        vec![
            contact(1, 0xada, |_| {}),
            account_record(2, |a| a.pinned_conversations = vec![]),
        ],
    );
    let ada = chat(0xada);
    for change in [
        ConversationChange::SetArchived { archived: true },
        ConversationChange::SetPinned { pinned: true },
        ConversationChange::SetMuted { until: Some(5) },
        ConversationChange::SetMarkedUnread {
            marked_unread: true,
        },
    ] {
        assert_eq!(
            w.click(change, &ada, 100),
            Err(Refused::SwitchOff),
            "refused with the switch off"
        );
    }
    assert!(w.intents().is_empty(), "no intent");
    assert_eq!(w.seq(), 0, "no seq taken");
    assert!(w.events().is_empty(), "the core is told nothing");
    assert_eq!(w.jobs(), 0, "no job");
    assert_eq!(
        Refused::SwitchOff.to_backend_error().kind,
        ErrorKind::Unsupported
    );

    // The control: the very same click with the switch on.
    w.switch(true);
    assert_eq!(w.archive(&ada, true, 200), Ok(()));
    assert_eq!(w.intents().len(), 1);
    assert_eq!(w.seq(), 1);
    assert_eq!(w.jobs(), 1);
}

/// B1: the automatic clear of a mark with the switch off is ACKNOWLEDGED (the core must not keep
/// the row in `pending_commands`) and dropped: nothing is stored, no job.
#[test]
fn with_the_switch_off_the_automatic_clear_is_acked_and_dropped() {
    let w = World::new();
    // The phone says marked, merged before the reading: with the switch on this clear is filed.
    w.put(10, vec![contact(1, 0xada, |c| c.marked_unread = true)]);
    let ada = chat(0xada);

    assert!(!w.durable(7, &ada, 50, 60), "no job to wake");
    let events = w.events();
    assert_eq!(acks(&events), vec![CmdId(7)], "acknowledged");
    assert_eq!(events.len(), 1, "and nothing else is told");
    assert!(w.intents().is_empty(), "nothing is stored");
    assert_eq!(w.jobs(), 0);

    w.switch(true);
    assert!(w.durable(8, &ada, 50, 70), "filed: the job is to be woken");
    assert_eq!(acks(&w.events()), vec![CmdId(8)]);
    let filed = w.intent(&person(0xada), MARKED_UNREAD).expect("filed");
    assert!(filed.automatic);
    assert_eq!(filed.value, Some(0));
}

/// A chat the phone does not list, a person known only by a number, Note to Self's mute and a
/// change that is no roaming setting are refused with nothing stored (§3.13, M5).
#[test]
fn a_chat_the_phone_does_not_list_is_refused() {
    let w = World::on();
    w.put(
        10,
        vec![contact(1, 0xada, |_| {}), account_record(2, |_| {})],
    );
    // No record of this person.
    assert_eq!(w.archive(&chat(0xdee), true, 20), Err(Refused::NotListed));
    // A person known only by a number.
    let number = ConvKey::Direct(kourier_model::UserId::parse("e164:+15550123").unwrap());
    assert_eq!(w.archive(&number, true, 20), Err(Refused::NotListed));
    // A group the phone has no record of.
    assert_eq!(w.mute(&group_chat(9), Some(5), 20), Err(Refused::NotListed));
    // Note to Self has no mute.
    let own = ConvKey::Direct(aci_user_id(w.f.account.aci).unwrap());
    assert_eq!(w.mute(&own, Some(5), 20), Err(Refused::NotMutable));
    // Not a roaming setting.
    assert_eq!(
        w.click(
            ConversationChange::SetBlocked { blocked: true },
            &chat(0xada),
            20
        ),
        Err(Refused::NotRoaming)
    );
    assert!(w.intents().is_empty());
    assert_eq!(w.seq(), 0);
    assert!(w.events().is_empty());
    assert_eq!(w.jobs(), 0);
    // The pin limit is `TooLarge` (L6): `a_pin_over_the_limit_is_refused_as_too_large`.
    for refused in [Refused::NotListed, Refused::Unreadable, Refused::NotMutable] {
        assert_eq!(refused.to_backend_error().kind, ErrorKind::InvalidInput);
    }
}

/// A pin over the account's limit is refused as `TooLarge` (A10d L6; architecture §4.2: on
/// `ChangeConversation` that kind is the pin limit), so the window names the limit, as for the
/// core's own `Refusal::TooMany`, and not "the network failed" (what `InvalidInput` reads as).
/// Its fixed words stay and name no chat. Mutant: the kind back to `InvalidInput`.
#[test]
fn a_pin_over_the_limit_is_refused_as_too_large() {
    let error = Refused::TooManyPins.to_backend_error();
    assert_eq!(error.kind, ErrorKind::TooLarge);
    assert_eq!(
        error.detail,
        "the account has pinned as many chats as it may"
    );
}

/// Note to Self and a group are filed on their own records (`account`, `group:<hex>`), with the
/// mirror and the event of each.
#[test]
fn note_to_self_and_a_group_are_filed_on_their_own_records() {
    let w = World::on();
    w.put(
        10,
        vec![account_record(1, |_| {}), group_record(2, 0x31, |_| {})],
    );
    let own = ConvKey::Direct(aci_user_id(w.f.account.aci).unwrap());
    let g = group_chat(0x31);

    assert_eq!(w.archive(&own, true, 100), Ok(()));
    assert_eq!(w.mute(&g, Some(i64::MAX), 101), Ok(()));
    assert_eq!(w.mark(&g, true, 102), Ok(()));

    let nts = w.intent(ACCOUNT_TARGET, ARCHIVED).expect("Note to Self");
    assert_eq!(
        (nts.value, nts.base, nts.shown),
        (Some(1), Some(0), Some(0))
    );
    let mute = w.intent(&group_t(0x31), MUTED_UNTIL).expect("group mute");
    assert_eq!(
        (mute.value, mute.base, mute.shown),
        (Some(i64::MAX), None, None)
    );
    let mark = w.intent(&group_t(0x31), MARKED_UNREAD).expect("group mark");
    assert_eq!(
        (mark.value, mark.base, mark.shown),
        (Some(1), Some(0), Some(0))
    );

    let events = w.events();
    assert_eq!(patch_of(&events, &own).unwrap().archived, Some(true));
    let patches: Vec<ConversationPatch> = events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::ConversationUpsert(p) if p.key == g => Some(p.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(patches.len(), 2);
    assert_eq!(patches[0].muted_until, Patch::Set(i64::MAX));
    assert_eq!(patches[1].marked_unread, Some(true));
    // The mirrors: what the core was told.
    let own_sid = ServiceId::from(w.f.account.aci).service_id_string();
    let told =
        w.f.db
            .read(|c| peers::load(c, &own_sid))
            .unwrap()
            .roaming
            .unwrap();
    assert!(told.archived);
    let id = groups::id_of_conv(&g).unwrap();
    let row =
        w.f.db
            .read(|c| groups::load_by_id(c, &id))
            .unwrap()
            .unwrap();
    assert_eq!(
        (row.archived, row.marked_unread, row.muted_until),
        (false, true, Some(i64::MAX))
    );
}

// ---------------------------------------------------------------------------------------------
// Click back, in-flight, seq
// ---------------------------------------------------------------------------------------------

/// L6: a click back to what the core showed before the first click writes nothing: the intent is
/// gone, the core is told both clicks, and the mirror follows the last one.
#[test]
fn a_click_back_writes_nothing() {
    let w = World::on();
    w.put(
        10,
        vec![contact(1, 0xada, |_| {}), account_record(2, |_| {})],
    );
    let ada = chat(0xada);
    assert_eq!(w.archive(&ada, true, 100), Ok(()));
    assert_eq!(w.intents().len(), 1);
    assert!(w.told(0xada).unwrap().archived);
    assert_eq!(w.archive(&ada, false, 200), Ok(()));
    assert!(w.intents().is_empty(), "the archive intent is gone");
    assert!(!w.told(0xada).unwrap().archived);

    assert_eq!(w.mute(&ada, Some(900), 300), Ok(()));
    assert_eq!(w.mute(&ada, None, 301), Ok(()));
    assert_eq!(w.mark(&ada, true, 302), Ok(()));
    assert_eq!(w.mark(&ada, false, 303), Ok(()));
    assert_eq!(w.pin(&ada, true, 304), Ok(()));
    assert_eq!(w.pin(&ada, false, 305), Ok(()));
    assert!(w.intents().is_empty(), "every pair cancelled");

    // The core heard every click, in order.
    let archives: Vec<Option<bool>> = w
        .events()
        .iter()
        .filter_map(|e| match e {
            BackendEvent::ConversationUpsert(p) if p.key == ada => Some(p.archived),
            _ => None,
        })
        .filter(Option::is_some)
        .collect();
    assert_eq!(archives, vec![Some(true), Some(false)]);

    // A click to another value than `shown` keeps its intent.
    assert_eq!(w.mute(&ada, Some(900), 400), Ok(()));
    assert_eq!(w.mute(&ada, Some(901), 401), Ok(()));
    let mute = w.intent(&person(0xada), MUTED_UNTIL).unwrap();
    assert_eq!((mute.value, mute.shown), (Some(901), None));
}

/// X3: once a write in flight carries the field, a click back is a NEW intent whose base is the
/// value the write sets (and not the shortcut that deletes it).
#[test]
fn a_click_back_during_the_put_is_a_new_intent() {
    let w = World::on();
    w.put(10, vec![contact(1, 0xada, |_| {})]);
    let ada = chat(0xada);
    assert_eq!(w.archive(&ada, true, 100), Ok(()));
    let carried = w.intent(&person(0xada), ARCHIVED).unwrap();
    assert_eq!(carried.base, Some(0));
    // The job planned it and the PUT is on its way.
    w.in_flight(&format!(
        r#"[{{"target": "{}", "field": "archived", "value": 1, "seq": {}}}]"#,
        person(0xada),
        carried.seq
    ));
    // He unarchives during the PUT.
    assert_eq!(w.archive(&ada, false, 200), Ok(()));
    let back = w
        .intent(&person(0xada), ARCHIVED)
        .expect("a new intent, not deleted");
    assert_eq!(back.value, Some(0));
    assert_eq!(back.base, Some(1), "the value the write in flight sets");
    assert_eq!(back.shown, Some(1));
    assert!(back.seq > carried.seq, "newer than the carried one");
    // And archiving again is another intent over it.
    assert_eq!(w.archive(&ada, true, 300), Ok(()));
    let again = w.intent(&person(0xada), ARCHIVED).unwrap();
    assert_eq!((again.value, again.base), (Some(1), Some(1)));
    assert!(again.seq > back.seq);
}

/// Y5: a seq is never reused, whatever was deleted since: a click back, `drop_all`.
#[test]
fn seq_never_goes_back() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xada, |_| {}),
            contact(2, 0xb0b, |_| {}),
            contact(3, 0xc1, |_| {}),
            contact(4, 0xd2, |_| {}),
        ],
    );
    assert_eq!(w.archive(&chat(0xada), true, 100), Ok(()));
    assert_eq!(w.archive(&chat(0xb0b), true, 101), Ok(()));
    assert_eq!(
        w.archive(&chat(0xb0b), false, 102),
        Ok(()),
        "B is cancelled"
    );
    assert_eq!(w.archive(&chat(0xc1), true, 103), Ok(()));
    let c = w.intent(&person(0xc1), ARCHIVED).unwrap();
    assert_eq!(c.seq, 3, "not 2, which B had");
    assert_eq!(w.seq(), 3);
    let dropped =
        w.f.db
            .zone(|tx| drop_all(tx, &w.f.account, 104, &w.wake))
            .unwrap();
    assert_eq!(dropped, 2);
    assert_eq!(w.archive(&chat(0xd2), true, 105), Ok(()));
    assert_eq!(w.intent(&person(0xd2), ARCHIVED).unwrap().seq, 4);
}

/// A10a review F3: the core sends the durable clear of a reading and, right behind it, a plain
/// click that marks the same chat again. They are filed in the order received, so the final
/// intent says MARKED.
#[test]
fn a_fresh_mark_right_after_the_automatic_clear_wins() {
    let w = World::on();
    w.put(10, vec![contact(1, 0xada, |c| c.marked_unread = true)]);
    let ada = chat(0xada);
    assert!(w.durable(1, &ada, 50, 60));
    let clear = w.intent(&person(0xada), MARKED_UNREAD).unwrap();
    assert!(clear.automatic);
    assert_eq!(clear.value, Some(0));
    assert_eq!(w.mark(&ada, true, 61), Ok(()));
    let marks: Vec<Intent> = w
        .intents()
        .into_iter()
        .filter(|i| i.field == MARKED_UNREAD)
        .collect();
    assert_eq!(marks.len(), 1, "one row per (target, field)");
    let last = &marks[0];
    assert_eq!(
        last.value,
        Some(1),
        "MARKED is what the phone is to be told"
    );
    assert!(!last.automatic, "it is the click now");
    assert!(last.seq > clear.seq, "in the order received");
    assert_eq!(last.base, Some(1));
}

// ---------------------------------------------------------------------------------------------
// Pins
// ---------------------------------------------------------------------------------------------

/// M3: the limit counts the stored record's ENTRIES (a group not fetched yet included) and the
/// live pin intents; the limit is the remote config's, else 4.
#[test]
fn the_pin_limit_counts_the_stored_entries() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xc1, |_| {}),
            contact(2, 0xc2, |_| {}),
            contact(3, 0xc3, |_| {}),
            contact(4, 0xd4, |_| {}),
            contact(5, 0xe5, |_| {}),
            // An unfetched group and three chats: four entries, the core lists three.
            account_record(6, |a| {
                a.pinned_conversations = vec![
                    group_pin(0x77),
                    contact_pin(0xc1),
                    contact_pin(0xc2),
                    contact_pin(0xc3),
                ];
            }),
        ],
    );
    assert_eq!(w.pin(&chat(0xd4), true, 100), Err(Refused::TooManyPins));
    assert!(w.intents().is_empty());
    assert!(w.events().is_empty());
    assert_eq!(w.seq(), 0);

    // The limit is the remote config's.
    w.set_pin_limit("5");
    assert_eq!(w.pin(&chat(0xd4), true, 101), Ok(()));
    let list = pin_lists(&w.events());
    assert_eq!(
        list,
        vec![vec![chat(0xc1), chat(0xc2), chat(0xc3), chat(0xd4)]],
        "the list the core is told: the record's order, the new pin last (the unfetched group is not named)"
    );
    // The live intent counts too.
    assert_eq!(w.pin(&chat(0xe5), true, 102), Err(Refused::TooManyPins));
    // Archiving a pinned chat unpins it: the slot is free again.
    assert_eq!(w.archive(&chat(0xc1), true, 103), Ok(()));
    assert_eq!(w.pin(&chat(0xe5), true, 104), Ok(()));
    // A chat that is pinned already adds none.
    assert_eq!(w.pin(&chat(0xe5), true, 105), Ok(()));
    // A limit that is no number is 4.
    assert_eq!(pin_limit_of(Some("many")), 4);
    assert_eq!(pin_limit_of(None), 4);
    assert_eq!(pin_limit_of(Some(" 6 ")), 6);
}

/// Signal's couplings: archiving a pinned chat unpins it, pinning an archived chat unarchives it;
/// the core hears the whole list.
#[test]
fn the_couplings_follow_signals_rules() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xada, |_| {}),
            contact(2, 0xb0b, |c| c.archived = true),
            account_record(3, |a| a.pinned_conversations = vec![contact_pin(0xada)]),
        ],
    );
    // Archive the pinned chat.
    assert_eq!(w.archive(&chat(0xada), true, 100), Ok(()));
    let unpin = w
        .intent(ACCOUNT_TARGET, &pin_field(&person(0xada)))
        .expect("also unpinned");
    assert_eq!(
        (unpin.value, unpin.base, unpin.shown),
        (Some(0), Some(1), Some(1))
    );
    let events = w.events();
    assert_eq!(
        patch_of(&events, &chat(0xada)).unwrap().archived,
        Some(true)
    );
    assert_eq!(pin_lists(&events), vec![Vec::<ConvKey>::new()]);
    // Pin the archived chat.
    assert_eq!(w.pin(&chat(0xb0b), true, 101), Ok(()));
    let unarchive = w.intent(&person(0xb0b), ARCHIVED).expect("also unarchived");
    assert_eq!(
        (unarchive.value, unarchive.base, unarchive.shown),
        (Some(0), Some(1), Some(1))
    );
    assert!(
        w.intent(ACCOUNT_TARGET, &pin_field(&person(0xb0b)))
            .is_some()
    );
    assert!(!w.told(0xb0b).unwrap().archived);
    let events = w.events();
    assert_eq!(
        patch_of(&events, &chat(0xb0b)).unwrap().archived,
        Some(false)
    );
    assert_eq!(pin_lists(&events), vec![vec![chat(0xb0b)]]);
    // Pinning the archived-and-unpinned one back is a net zero: both of its intents are gone.
    assert_eq!(w.pin(&chat(0xada), true, 102), Ok(()));
    assert!(
        w.intent(ACCOUNT_TARGET, &pin_field(&person(0xada)))
            .is_none()
    );
}

// ---------------------------------------------------------------------------------------------
// The automatic clear
// ---------------------------------------------------------------------------------------------

/// N-D: the clear is filed only when the newest stored record, or a live intent, says marked.
#[test]
fn the_automatic_clear_is_not_filed_when_the_phone_has_no_mark() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xada, |_| {}),
            contact(2, 0xb0b, |c| c.marked_unread = true),
            contact(3, 0xc1, |_| {}),
        ],
    );
    // The phone has no mark: nothing to clear, nothing stored, but the command is acknowledged.
    assert_eq!(w.automatic(&chat(0xada), 50, 60), Automatic::Dropped);
    assert!(!w.durable(1, &chat(0xada), 50, 61));
    assert_eq!(acks(&w.events()), vec![CmdId(1)]);
    assert!(w.intents().is_empty());
    assert_eq!(w.jobs(), 0);
    // The phone has one: filed.
    assert_eq!(w.automatic(&chat(0xb0b), 50, 62), Automatic::Filed);
    let filed = w.intent(&person(0xb0b), MARKED_UNREAD).unwrap();
    assert_eq!(
        (filed.value, filed.base, filed.shown),
        (Some(0), Some(1), Some(0))
    );
    assert!(filed.automatic);
    assert_eq!(w.jobs(), 1);
    // Cleared once, the next reading finds nothing to clear (the intent says unmarked).
    assert_eq!(w.automatic(&chat(0xb0b), 90, 91), Automatic::Dropped);
    // A click not written yet counts as a mark: the reading cancels it (a net zero).
    assert_eq!(w.mark(&chat(0xc1), true, 100), Ok(()));
    assert_eq!(w.automatic(&chat(0xc1), 110, 111), Automatic::Filed);
    assert!(w.intent(&person(0xc1), MARKED_UNREAD).is_none());
}

/// N-C: a mark the phone merged AFTER the reading stays: nothing is filed and the core is told
/// the chat is marked again; a mark merged before the reading is cleared.
#[test]
fn a_mark_newer_than_the_reading_stays() {
    let w = World::on();
    // Merged at 200 (the backend's clock).
    w.put(200, vec![contact(1, 0xada, |c| c.marked_unread = true)]);
    let ada = chat(0xada);
    // The user read the chat at 100: the phone marked it at 200, after.
    assert_eq!(w.automatic(&ada, 100, 250), Automatic::PhoneKept);
    assert!(w.intents().is_empty());
    assert_eq!(w.jobs(), 0);
    let told = patch_of(&w.events(), &ada).expect("the core is told");
    assert_eq!(told.marked_unread, Some(true));
    // The same record, read at 300: the mark is older than the reading.
    assert_eq!(w.automatic(&ada, 300, 350), Automatic::Filed);
    assert!(w.intent(&person(0xada), MARKED_UNREAD).is_some());
    assert!(
        w.events().is_empty(),
        "the core cleared its own row already"
    );
}

/// Review L2: a "Mark as Read" click of his that is not written yet rules. The phone's record is
/// merged after the reading for an unrelated field (a name), so its `updated_at` is newer than
/// the reading, and its mark is the OLD one (equal to the click's base: the merge kept the click).
/// N-C must not mistake that for a mark the phone made after the reading: no `marked_unread: true`
/// is told to the core, the click stays, and nothing is filed over it.
#[test]
fn a_click_of_his_rules_over_the_phones_newer_record() {
    let w = World::on();
    w.put(10, vec![contact(1, 0xada, |c| c.marked_unread = true)]);
    let ada = chat(0xada);
    assert_eq!(
        w.mark(&ada, false, 20),
        Ok(()),
        "Mark as Read, not written yet"
    );
    let click = w.intent(&person(0xada), MARKED_UNREAD).unwrap();
    assert_eq!(
        (click.value, click.base, click.automatic),
        (Some(0), Some(1), false)
    );
    // The phone's record, rotated for a name, with the old mark; merged at 200.
    w.merge(
        200,
        vec![contact(2, 0xada, |c| {
            c.marked_unread = true;
            c.given_name = "A".to_owned();
        })],
    );
    assert_eq!(
        w.intent(&person(0xada), MARKED_UNREAD),
        Some(click.clone()),
        "kept"
    );
    w.events();

    // The chat was read at 100 (before the merge at 200): the click rules.
    assert_eq!(w.automatic(&ada, 100, 250), Automatic::Dropped);
    assert!(
        patch_of(&w.events(), &ada).is_none_or(|p| p.marked_unread.is_none()),
        "the core is not told the phone's old mark"
    );
    assert_eq!(
        w.intent(&person(0xada), MARKED_UNREAD),
        Some(click),
        "still his click"
    );
    assert_eq!(w.jobs(), 1, "only the click's job");
}

/// M5, the Signal side of "ONE clock". The chain in production: `asked_at` is the core's
/// `Shared::now()` (`runtime.rs`, the clock `CoreConfig` was started with, tested by
/// `kourier-core` `roaming_clear.rs`); the supervisor hands that very clock to the backend as
/// `AccountContext::clock` (`supervisor.rs` `ContextParams { clock: self.sh.clock.clone() }`,
/// tested by `kourier-core` `one_clock::the_backend_reads_the_clock_the_core_was_started_with`);
/// the Signal actor keeps it as `Shared::clock` (`actor.rs` `AccountContext { clock, .. }`) and
/// `storage::sync` stamps every merge zone with `shared.now()`. Here: with the merge stamped by
/// a clock and the reading taken on THE SAME clock, the rule compares them as it says: a mark
/// merged before the reading is cleared, one merged in the same millisecond too (`>` is
/// strict), one merged after stays.
#[test]
fn the_merge_time_and_the_cores_asked_at_read_one_clock() {
    use kourier_backend_api::{Clock as _, FakeClock};

    // One clock, as the supervisor hands it to both sides.
    let clock = FakeClock::new(1_900_000_000_000);
    let now = |clock: &FakeClock| i64::try_from(clock.now_ms()).unwrap();
    let w = World::on();

    let merged_at = now(&clock);
    w.put(
        merged_at,
        vec![contact(1, 0xada, |c| c.marked_unread = true)],
    );
    let stamped: i64 =
        w.f.db
            .read(|c| -> Result<i64> {
                Ok(c.query_row("SELECT updated_at FROM storage_records", [], |r| r.get(0))?)
            })
            .unwrap();
    assert_eq!(
        stamped, merged_at,
        "the merge is stamped with the clock it was given"
    );

    let ada = chat(0xada);
    // The reading in the same millisecond as the merge: the clear stands.
    assert_eq!(
        w.automatic(&ada, now(&clock), now(&clock)),
        Automatic::Filed
    );
    w.f.db
        .zone(|tx| drop_all(tx, &w.f.account, now(&clock), &w.wake))
        .unwrap();
    // The reading a second after the merge: the clear stands.
    clock.advance(std::time::Duration::from_secs(1));
    assert_eq!(
        w.automatic(&ada, now(&clock), now(&clock)),
        Automatic::Filed
    );
    w.f.db
        .zone(|tx| drop_all(tx, &w.f.account, now(&clock), &w.wake))
        .unwrap();
    // The reading BEFORE the merge (the user read at `asked_at`, the phone marked a second
    // later): the phone's mark stays.
    let asked_at = now(&clock);
    clock.advance(std::time::Duration::from_secs(1));
    w.put(
        now(&clock),
        vec![contact(2, 0xada, |c| c.marked_unread = true)],
    );
    assert_eq!(
        w.automatic(&ada, asked_at, now(&clock)),
        Automatic::PhoneKept
    );
}

// ---------------------------------------------------------------------------------------------
// The identity intent
// ---------------------------------------------------------------------------------------------

/// M2: an identity intent stores its key and is written only onto a record that holds the same
/// key (or none); for another key it is dropped on the spot and nothing is stated.
#[test]
fn an_identity_intent_for_another_key_is_dropped() {
    let w = World::new();
    let key = vec![1u8; 33];
    w.put(
        10,
        vec![
            contact(1, 0xada, |c| {
                c.identity_key = vec![1u8; 33];
            }),
            contact(2, 0xb0b, |_| {}),
        ],
    );
    let sid = |n: u128| ServiceId::from(aci(n)).service_id_string();
    let file = |w: &World, who: u128, state: i64| {
        w.f.db
            .zone(|tx| intend_identity(tx, &w.f.account, &sid(who), state, 0, &key, 100))
            .unwrap()
            .is_filed()
    };
    // The switch is off: nothing is filed.
    assert!(!file(&w, 0xada, 1));
    assert!(w.intents().is_empty());
    w.switch(true);
    // A person the phone has no record of.
    assert!(!file(&w, 0xdee, 1));
    assert!(file(&w, 0xada, 1));
    assert!(file(&w, 0xb0b, 1));
    let intent = w.intent(&person(0xada), IDENTITY).unwrap();
    assert_eq!(intent.key.as_deref(), Some(&key[..]));
    assert_eq!(
        (intent.value, intent.base, intent.shown),
        (Some(1), Some(0), Some(0))
    );
    w.events();

    let identity_for_key = |target: &str, record_key: &[u8]| {
        w.f.db
            .zone(|tx| identity_for(tx, target, record_key))
            .unwrap()
    };
    // The same key: it applies, and stays.
    assert!(identity_for_key(&person(0xada), &key).is_some());
    assert!(w.intent(&person(0xada), IDENTITY).is_some());
    // A record with no key at all takes it (Desktop writes both fields).
    assert!(identity_for_key(&person(0xb0b), &[]).is_some());
    // Another key: dropped, and nothing is stated.
    assert!(identity_for_key(&person(0xada), &[2u8; 33]).is_none());
    assert!(w.intent(&person(0xada), IDENTITY).is_none(), "dropped");
    assert!(w.events().is_empty(), "nothing is stated");
    assert!(
        w.intent(&person(0xb0b), IDENTITY).is_some(),
        "the other stays"
    );
}

/// C5b: `intend_identity` tells whether the newest record holds the SAME key as the click (the
/// caller queues the Verified sync as well when it does not), and nothing else of the filing
/// changes: the intent is filed in every case but the two that file nothing.
#[test]
fn intend_identity_says_whether_the_record_holds_the_same_key() {
    let w = World::new();
    let key = vec![1u8; 33];
    w.put(
        10,
        vec![
            contact(1, 0xada, |c| c.identity_key = vec![1u8; 33]),
            contact(2, 0xb0b, |_| {}),
            contact(3, 0xc1, |c| c.identity_key = vec![2u8; 33]),
        ],
    );
    let sid = |n: u128| ServiceId::from(aci(n)).service_id_string();
    let file = |who: u128| {
        w.f.db
            .zone(|tx| intend_identity(tx, &w.f.account, &sid(who), 1, 0, &key, 100))
            .unwrap()
    };
    assert_eq!(file(0xada), IdentityFiled::NotFiled, "the switch is off");
    w.switch(true);
    assert_eq!(file(0xada), IdentityFiled::SameKey);
    assert_eq!(file(0xb0b), IdentityFiled::OtherKey, "a record with no key");
    assert_eq!(
        file(0xc1),
        IdentityFiled::OtherKey,
        "a record with another key"
    );
    assert_eq!(
        file(0xdee),
        IdentityFiled::NotFiled,
        "no record of the person"
    );
    for who in [0xada, 0xb0b, 0xc1] {
        assert!(
            w.intent(&person(who), IDENTITY).is_some(),
            "an intent is filed for the same key and for another"
        );
    }
    assert!(w.intent(&person(0xdee), IDENTITY).is_none());
    assert!(IdentityFiled::SameKey.is_filed() && IdentityFiled::OtherKey.is_filed());
    assert!(!IdentityFiled::NotFiled.is_filed());
}

// ---------------------------------------------------------------------------------------------
// Dropping
// ---------------------------------------------------------------------------------------------

/// BQ10 + Y6: turning the switch off deletes the intents that no write in flight carries and
/// restates the STORED values of what it dropped; the carried one and the in-flight row stay; a
/// later click on a carried field is deleted without a restatement (the landing states it).
#[test]
fn turning_off_drops_the_intents() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xada, |_| {}),
            contact(2, 0xb0b, |_| {}),
            group_record(3, 0x42, |_| {}),
            account_record(4, |_| {}),
        ],
    );
    let g = group_chat(0x42);
    assert_eq!(w.archive(&chat(0xada), true, 100), Ok(()));
    assert_eq!(w.mute(&chat(0xb0b), Some(9_999), 101), Ok(()));
    assert_eq!(w.mark(&g, true, 102), Ok(()));
    assert_eq!(w.archive(&g, true, 103), Ok(()));
    let carried_mark = w.intent(&group_t(0x42), MARKED_UNREAD).unwrap();
    let carried_archive = w.intent(&group_t(0x42), ARCHIVED).unwrap();
    // The write in flight carries the group's two intents.
    w.in_flight(&format!(
        r#"[{{"target": "{t}", "field": "marked_unread", "value": 1, "seq": {a}}},
            {{"target": "{t}", "field": "archived", "value": 1, "seq": {b}}}]"#,
        t = group_t(0x42),
        a = carried_mark.seq,
        b = carried_archive.seq
    ));
    // He unarchives the group during the PUT: a newer intent on a carried field.
    assert_eq!(w.archive(&g, false, 104), Ok(()));
    w.events();

    // The switch goes off.
    w.switch(false);
    let dropped =
        w.f.db
            .zone(|tx| drop_all(tx, &w.f.account, 200, &w.wake))
            .unwrap();
    assert_eq!(
        dropped, 3,
        "ada's archive, bob's mute, the group's newer click"
    );
    let left = w.intents();
    assert_eq!(left.len(), 1, "only the carried mark stays");
    assert_eq!(left[0].field, MARKED_UNREAD);
    assert_eq!(left[0].seq, carried_mark.seq);
    assert_eq!(w.in_flight_rows(), 1, "the in-flight row stays");

    let events = w.events();
    let ada = patch_of(&events, &chat(0xada)).expect("ada's stored value is restated");
    assert_eq!(ada.archived, Some(false));
    let bob = patch_of(&events, &chat(0xb0b)).expect("bob's stored value is restated");
    assert_eq!(bob.muted_until, Patch::Clear);
    assert!(
        patch_of(&events, &g).is_none(),
        "the landing of the write in flight states the group's values"
    );
    // The mirrors went back with the restatement.
    assert!(!w.told(0xada).unwrap().archived);
    assert_eq!(w.told(0xb0b).unwrap().muted_until, None);
}

/// Y3 (3): an intent older than 30 days is dropped with the same restatement.
#[test]
fn an_old_intent_is_dropped_and_restated() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xada, |_| {}),
            contact(2, 0xb0b, |_| {}),
            contact(3, 0xc1, |_| {}),
        ],
    );
    let day = 24 * 3600 * 1000;
    let now = 100 * day;
    assert_eq!(w.archive(&chat(0xada), true, now - 31 * day), Ok(()));
    assert_eq!(w.mute(&chat(0xb0b), Some(5), now - 11 * day), Ok(()));
    // Exactly 30 days old is not older than 30 days.
    assert_eq!(w.mark(&chat(0xc1), true, now - STALE_AFTER_MS), Ok(()));
    w.events();
    let dropped =
        w.f.db
            .zone(|tx| drop_stale(tx, &w.f.account, now, &w.wake))
            .unwrap();
    assert_eq!(dropped, 1);
    assert!(w.intent(&person(0xada), ARCHIVED).is_none());
    assert!(w.intent(&person(0xb0b), MUTED_UNTIL).is_some());
    assert!(w.intent(&person(0xc1), MARKED_UNREAD).is_some());
    let events = w.events();
    assert_eq!(
        patch_of(&events, &chat(0xada)).unwrap().archived,
        Some(false),
        "the phone's value"
    );
    assert!(!w.told(0xada).unwrap().archived);
    assert!(patch_of(&events, &chat(0xb0b)).is_none());
    // Nothing is older: nothing is dropped.
    assert_eq!(
        w.f.db
            .zone(|tx| drop_stale(tx, &w.f.account, now, &w.wake))
            .unwrap(),
        0
    );
}

// ---------------------------------------------------------------------------------------------
// A message overtakes an Archive click (critic X1)
// ---------------------------------------------------------------------------------------------

/// X1 (2), BQ1 (a): a message brings the chat back (A1's `leaves_the_archive`); a live Archive
/// intent that is not written yet is deleted, and one that the write in flight carries gets a
/// new intent `archived = false` after it. A direct chat and a group.
#[test]
fn a_message_in_the_archive_drops_the_archive_intent() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xada, |_| {}),
            contact(2, 0xb0b, |_| {}),
            group_record(3, 0x55, |_| {}),
        ],
    );
    let unarchive = |conv: &ConvKey| {
        w.f.db
            .zone(|tx| {
                crate::receive::unarchive::after_own_send(tx, &w.f.account, 500, conv, &w.wake)
            })
            .unwrap();
    };

    // Not written yet: deleted.
    assert_eq!(w.archive(&chat(0xada), true, 100), Ok(()));
    w.events();
    unarchive(&chat(0xada));
    assert!(w.intent(&person(0xada), ARCHIVED).is_none(), "overtaken");
    assert_eq!(
        patch_of(&w.events(), &chat(0xada)).unwrap().archived,
        Some(false),
        "the core hears the unarchive"
    );

    // The group the same way.
    let g = group_chat(0x55);
    assert_eq!(w.archive(&g, true, 101), Ok(()));
    unarchive(&g);
    assert!(w.intent(&group_t(0x55), ARCHIVED).is_none());

    // Carried by the write in flight: that PUT archives the chat on the phone, so a new intent
    // follows it.
    assert_eq!(w.archive(&chat(0xb0b), true, 102), Ok(()));
    let carried = w.intent(&person(0xb0b), ARCHIVED).unwrap();
    w.in_flight(&format!(
        r#"[{{"target": "{}", "field": "archived", "value": 1, "seq": {}}}]"#,
        person(0xb0b),
        carried.seq
    ));
    unarchive(&chat(0xb0b));
    let after = w.intent(&person(0xb0b), ARCHIVED).expect("a new intent");
    assert_eq!(after.value, Some(0));
    assert_eq!(after.base, Some(1), "the value the write sets");
    assert!(after.seq > carried.seq);
    assert!(w.jobs() >= 1);
}

/// B6's open item 1: a message that brings back a chat whose Archive is carried by the write in
/// flight files no new intent while the switch is OFF (the write job would never write it, so
/// it would linger until the 30-day drop). The carried intent and the job stay as they were.
#[test]
fn a_message_in_the_archive_files_nothing_while_the_switch_is_off() {
    let w = World::on();
    w.put(10, vec![contact(1, 0xb0b, |_| {})]);
    assert_eq!(w.archive(&chat(0xb0b), true, 102), Ok(()));
    let carried = w.intent(&person(0xb0b), ARCHIVED).unwrap();
    w.in_flight(&format!(
        r#"[{{"target": "{}", "field": "archived", "value": 1, "seq": {}}}]"#,
        person(0xb0b),
        carried.seq
    ));
    let jobs = w.jobs();
    let job_at = w.job_at();
    w.switch(false);
    w.f.db
        .zone(|tx| {
            crate::receive::unarchive::after_own_send(tx, &w.f.account, 500, &chat(0xb0b), &w.wake)
        })
        .unwrap();
    let after = w.intent(&person(0xb0b), ARCHIVED).unwrap();
    assert_eq!(
        (after.value, after.seq),
        (Some(1), carried.seq),
        "the carried intent is as it was: no archived = false follows it"
    );
    assert_eq!((w.jobs(), w.job_at()), (jobs, job_at), "no job is made due");
    assert_eq!(w.in_flight_rows(), 1, "the write in flight is not touched");
}

/// Critic Y3: the intents of an earlier run are not orphaned when their job row is gone (a dropped
/// job, a deleted row): a start queues `storage_write` again, due a second later, and does nothing
/// when no intent is stored.
#[test]
fn a_start_queues_the_write_job_for_intents_left_behind() {
    let w = World::on();
    w.put(10, vec![contact(1, 0xada, |_| {})]);
    let start = |at: i64| {
        w.f.db
            .zone(|tx| crate::maintenance::enqueue_at_start(tx, at))
            .unwrap();
    };
    let drop_job = || {
        w.f.db
            .zone(|tx| -> Result<()> {
                tx.execute("DELETE FROM jobs WHERE kind = 'storage_write'", [])?;
                Ok(())
            })
            .unwrap();
    };

    // No intent: a start queues the other jobs and not this one.
    start(100);
    assert_eq!(w.jobs(), 0, "nothing to write, no write job");

    // An intent whose job row is gone: the next start brings the job back.
    assert_eq!(w.archive(&chat(0xada), true, 200), Ok(()));
    drop_job();
    assert_eq!(w.jobs(), 0);
    start(50_000);
    assert_eq!(w.jobs(), 1, "the intent has its job again");
    assert_eq!(w.job_at(), Some(51_000), "due a second after the start");

    // A row that is already there (a back-off further out) is not pulled forward.
    w.f.db
        .zone(|tx| -> Result<()> {
            tx.execute(
                "UPDATE jobs SET next_at = 900000 WHERE kind = 'storage_write'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    start(60_000);
    assert_eq!((w.jobs(), w.job_at()), (1, Some(900_000)));
}

// ---------------------------------------------------------------------------------------------
// The job row
// ---------------------------------------------------------------------------------------------

/// Every change makes the `storage_write` job due a second after the LAST change (a trailing
/// debounce), and the helper never pulls a later time forward (critic Y4): a back-off or a hold
/// stands.
#[test]
fn the_write_job_is_debounced_and_never_pulled_forward() {
    let w = World::on();
    w.put(
        10,
        vec![contact(1, 0xada, |_| {}), contact(2, 0xb0b, |_| {})],
    );
    assert_eq!(w.archive(&chat(0xada), true, 1_000), Ok(()));
    assert_eq!(w.job_at(), Some(2_000));
    assert_eq!(w.mark(&chat(0xb0b), true, 1_500), Ok(()));
    assert_eq!(w.job_at(), Some(2_500), "pushed later by the next change");
    assert_eq!(w.jobs(), 1, "one row");
    // A back-off to 60 s: a click does not make it due earlier.
    w.f.db
        .zone(|tx| -> Result<()> {
            tx.execute(
                "UPDATE jobs SET next_at = 60000 WHERE kind = 'storage_write'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(w.mute(&chat(0xada), Some(7), 2_000), Ok(()));
    assert_eq!(w.job_at(), Some(60_000));
    // A hold of the kind bounds a new row too.
    w.f.db
        .zone(|tx| -> Result<()> {
            tx.execute("DELETE FROM jobs", [])?;
            jobs::pause_kind(tx, JobKind::StorageWrite, 3_000, 90_000)?;
            Ok(())
        })
        .unwrap();
    assert_eq!(w.mark(&chat(0xb0b), false, 3_000), Ok(()));
    assert_eq!(w.job_at(), Some(90_000));
}

/// The log-safe `Debug` of an intent shows no chat, key or value.
#[test]
fn an_intent_prints_no_chat() {
    let w = World::on();
    w.put(10, vec![contact(1, 0xada, |_| {})]);
    assert_eq!(w.archive(&chat(0xada), true, 100), Ok(()));
    let shown = format!("{:?}", w.intent(&person(0xada), ARCHIVED).unwrap());
    assert!(
        shown.contains("archived") && shown.contains("seq"),
        "{shown}"
    );
    assert!(
        !shown.contains("ada") && !shown.contains("contact:"),
        "{shown}"
    );
}

// ---------------------------------------------------------------------------------------------
// The three-way rule in the merges (A10b chunk B4, §3.3)
// ---------------------------------------------------------------------------------------------

/// A mute that is ours and a mute that is theirs, in ms.
const OURS: i64 = 900;
const THEIRS: i64 = 5_000;

/// The pure rule, row by row (and `shown` is not read: the same verdict whatever it says).
#[test]
fn three_way_the_table_of_the_rule() {
    let intent = |value, base, shown| Intent {
        target: "contact:x".to_owned(),
        field: MUTED_UNTIL.to_owned(),
        value,
        base,
        shown,
        key: None,
        seq: 1,
        automatic: false,
        created_at: 0,
    };
    // (ours, base, theirs, verdict)
    let rows = [
        // theirs == base: the phone did not touch the field.
        (Some(1), Some(0), Some(0), Verdict::Stays),
        (Some(0), Some(1), Some(1), Verdict::Stays),
        (Some(1), Some(1), Some(1), Verdict::Stays),
        (Some(OURS), None, None, Verdict::Stays),
        (None, Some(OURS), Some(OURS), Verdict::Stays),
        // theirs != base and theirs == ours: done elsewhere.
        (Some(1), Some(0), Some(1), Verdict::DoneElsewhere),
        (Some(0), Some(1), Some(0), Verdict::DoneElsewhere),
        (Some(OURS), None, Some(OURS), Verdict::DoneElsewhere),
        (None, Some(OURS), None, Verdict::DoneElsewhere),
        // theirs != base and theirs != ours: the phone wins.
        (Some(1), Some(1), Some(0), Verdict::PhoneWins),
        (Some(1), Some(0), Some(2), Verdict::PhoneWins),
        (Some(OURS), None, Some(THEIRS), Verdict::PhoneWins),
        (None, Some(OURS), Some(THEIRS), Verdict::PhoneWins),
        (Some(OURS), Some(THEIRS), None, Verdict::PhoneWins),
    ];
    for (ours, base, theirs, verdict) in rows {
        for shown in [None, Some(0), Some(1), Some(OURS), Some(THEIRS)] {
            assert_eq!(
                three_way(&intent(ours, base, shown), theirs),
                verdict,
                "ours {ours:?}, base {base:?}, shown {shown:?}, theirs {theirs:?}"
            );
        }
    }
}

// ----- contact -----

/// Contact, row 1: the phone rotates the record for another reason (a block) and its flags are
/// what they were: the three intents STAY, the merge states none of the three flags (the
/// block is stated), and the told base keeps OURS. The record of another person changes nothing.
#[test]
fn three_way_contact_stays() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xada, |_| {}),
            contact(2, 0xb0b, |_| {}),
            account_record(3, |_| {}),
        ],
    );
    let ada = chat(0xada);
    assert_eq!(w.archive(&ada, true, 100), Ok(()));
    assert_eq!(w.mark(&ada, true, 101), Ok(()));
    assert_eq!(w.mute(&ada, Some(OURS), 102), Ok(()));
    w.events();

    let events = w.merge(
        200,
        vec![
            contact(4, 0xada, |c| c.blocked = true),
            contact(5, 0xb0b, |c| {
                c.archived = true;
                c.marked_unread = true;
                c.muted_until_timestamp = 77;
            }),
        ],
    );
    let patch = patch_of(&events, &ada).expect("the block is stated");
    assert_eq!(patch.blocked, Some(true));
    assert_eq!((patch.archived, patch.marked_unread), (None, None));
    assert_eq!(patch.muted_until, Patch::Unchanged);
    assert_eq!(w.intents().len(), 3, "every intent stays");
    assert_eq!(
        w.told(0xada),
        Some(Roaming {
            archived: true,
            muted_until: Some(OURS)
        }),
        "the told base keeps ours"
    );
    // Bob has no intent: his record is stated as it always was.
    let bob = patch_of(&events, &chat(0xb0b)).expect("bob's flags");
    assert_eq!(
        (bob.archived, bob.marked_unread, bob.muted_until),
        (Some(true), Some(true), Patch::Set(77))
    );
}

/// Contact, row 2: the phone did what we did (archive, mark, mute): the intents are deleted and
/// NOTHING is stated.
#[test]
fn three_way_contact_done_elsewhere() {
    let w = World::on();
    w.put(
        10,
        vec![contact(1, 0xada, |_| {}), account_record(2, |_| {})],
    );
    let ada = chat(0xada);
    assert_eq!(w.archive(&ada, true, 100), Ok(()));
    assert_eq!(w.mark(&ada, true, 101), Ok(()));
    assert_eq!(w.mute(&ada, Some(OURS), 102), Ok(()));
    w.events();

    let events = w.merge(
        200,
        vec![contact(3, 0xada, |c| {
            c.archived = true;
            c.marked_unread = true;
            c.muted_until_timestamp = OURS as u64;
        })],
    );
    assert!(
        patch_of(&events, &ada).is_none(),
        "the core already shows it: nothing is stated"
    );
    assert!(w.intents().is_empty(), "the intents are closed");
    assert_eq!(
        w.told(0xada),
        Some(Roaming {
            archived: true,
            muted_until: Some(OURS)
        })
    );
}

/// Contact, row 3: the phone wins. A mute the phone set to another time, and an Archive intent
/// whose base already says archived (ours == base: what a landing leaves for an Archive clicked
/// again during the PUT, H3/X3; staged here by a click while the told base says archived, since
/// review H2 an Archive over A1's moved told base has base 0 and a phone "not archived" leaves it
/// alone) that the phone then unarchived: both intents are deleted, the phone's values are stated,
/// and the told base moves.
#[test]
fn three_way_contact_the_phone_wins() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xada, |c| c.archived = true),
            account_record(2, |_| {}),
        ],
    );
    let ada = chat(0xada);
    assert_eq!(w.archive(&ada, true, 100), Ok(()));
    assert_eq!(w.mute(&ada, Some(OURS), 101), Ok(()));
    let archive = w.intent(&person(0xada), ARCHIVED).unwrap();
    assert_eq!((archive.value, archive.base), (Some(1), Some(1)));
    w.events();

    let events = w.merge(
        200,
        vec![contact(3, 0xada, |c| {
            c.archived = false;
            c.muted_until_timestamp = THEIRS as u64;
        })],
    );
    let patch = patch_of(&events, &ada).expect("the phone's values are stated");
    assert_eq!(patch.archived, Some(false));
    assert_eq!(patch.muted_until, Patch::Set(THEIRS));
    assert!(w.intents().is_empty(), "both intents are deleted");
    assert_eq!(
        w.told(0xada),
        Some(Roaming {
            archived: false,
            muted_until: Some(THEIRS)
        }),
        "the told base moves to theirs"
    );
}

/// A mark can be overruled too: the mark intent of a chat whose record was marked already
/// (ours == base) is deleted when the phone clears the mark, and the phone's value is stated.
#[test]
fn three_way_contact_the_phone_wins_a_mark() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xada, |c| c.marked_unread = true),
            account_record(2, |_| {}),
        ],
    );
    let ada = chat(0xada);
    assert_eq!(w.mark(&ada, true, 100), Ok(()));
    let mark = w.intent(&person(0xada), MARKED_UNREAD).unwrap();
    assert_eq!((mark.value, mark.base), (Some(1), Some(1)));
    w.events();
    let events = w.merge(200, vec![contact(3, 0xada, |_| {})]);
    assert_eq!(
        patch_of(&events, &ada).expect("stated").marked_unread,
        Some(false)
    );
    assert!(w.intents().is_empty());
}

// ----- group -----

/// Group, row 1: the record is rotated for a block and its flags are what they were: the
/// intents stay, the `groups` mirror keeps OUR values (the merge used to overwrite it from the
/// record: critic M3), and only the block is stated.
#[test]
fn three_way_group_stays() {
    let w = World::on();
    w.put(
        10,
        vec![group_record(1, 0x31, |_| {}), account_record(2, |_| {})],
    );
    w.core_has_group(0x31);
    let g = group_chat(0x31);
    assert_eq!(w.archive(&g, true, 100), Ok(()));
    assert_eq!(w.mark(&g, true, 101), Ok(()));
    assert_eq!(w.mute(&g, Some(OURS), 102), Ok(()));
    w.events();

    let events = w.merge(200, vec![group_record(3, 0x31, |r| r.blocked = true)]);
    let patch = patch_of(&events, &g).expect("the block is stated");
    assert_eq!(patch.blocked, Some(true));
    assert_eq!((patch.archived, patch.marked_unread), (None, None));
    assert_eq!(patch.muted_until, Patch::Unchanged);
    assert_eq!(w.intents().len(), 3);
    assert_eq!(
        w.group_mirror(0x31),
        (true, true, Some(OURS)),
        "the mirror keeps ours"
    );
}

/// Group, row 2: the phone did what we did: the intents are deleted, nothing is stated, the
/// mirror is the same value.
#[test]
fn three_way_group_done_elsewhere() {
    let w = World::on();
    w.put(
        10,
        vec![group_record(1, 0x31, |_| {}), account_record(2, |_| {})],
    );
    w.core_has_group(0x31);
    let g = group_chat(0x31);
    assert_eq!(w.archive(&g, true, 100), Ok(()));
    assert_eq!(w.mark(&g, true, 101), Ok(()));
    assert_eq!(w.mute(&g, Some(OURS), 102), Ok(()));
    w.events();

    let events = w.merge(
        200,
        vec![group_record(3, 0x31, |r| {
            r.archived = true;
            r.marked_unread = true;
            r.muted_until_timestamp = OURS as u64;
        })],
    );
    assert!(patch_of(&events, &g).is_none(), "nothing is stated");
    assert!(w.intents().is_empty());
    assert_eq!(w.group_mirror(0x31), (true, true, Some(OURS)));
}

/// Group, row 3: the phone wins a mute set to another time and an Archive intent whose base
/// already says archived (ours == base, staged as in `three_way_contact_the_phone_wins`; review H2
/// changed it from an Archive over A1's moved mirror): deleted, stated, and the mirror takes the
/// phone's values.
#[test]
fn three_way_group_the_phone_wins() {
    let w = World::on();
    w.put(
        10,
        vec![
            group_record(1, 0x31, |r| r.archived = true),
            account_record(2, |_| {}),
        ],
    );
    w.core_has_group(0x31);
    let g = group_chat(0x31);
    assert_eq!(w.archive(&g, true, 100), Ok(()));
    let archive = w.intent(&group_t(0x31), ARCHIVED).unwrap();
    assert_eq!((archive.value, archive.base), (Some(1), Some(1)));
    assert_eq!(w.mute(&g, Some(OURS), 101), Ok(()));
    w.events();

    let events = w.merge(
        200,
        vec![group_record(3, 0x31, |r| {
            r.archived = false;
            r.muted_until_timestamp = THEIRS as u64;
        })],
    );
    let patch = patch_of(&events, &g).expect("the phone's values are stated");
    assert_eq!(patch.archived, Some(false));
    assert_eq!(patch.muted_until, Patch::Set(THEIRS));
    assert!(w.intents().is_empty());
    assert_eq!(w.group_mirror(0x31), (false, false, Some(THEIRS)));
}

// ----- account (Note to Self) -----

/// Account, row 1: an account record that leaves Note to Self as it was (the phone changed a
/// setting): the archive and mark intents stay, nothing is stated (the old diff against the
/// told base would un-archive the chat here), the told base keeps OURS.
#[test]
fn three_way_account_stays() {
    let w = World::on();
    w.put(10, vec![account_record(1, |_| {})]);
    let own = ConvKey::Direct(aci_user_id(w.f.account.aci).unwrap());
    assert_eq!(w.archive(&own, true, 100), Ok(()));
    assert_eq!(w.mark(&own, true, 101), Ok(()));
    w.events();

    let events = w.merge(200, vec![account_record(2, |a| a.read_receipts = true)]);
    assert!(
        patch_of(&events, &own).is_none(),
        "Note to Self is not touched"
    );
    assert_eq!(w.intents().len(), 2);
    assert_eq!(
        w.told_own(),
        Some(Roaming {
            archived: true,
            muted_until: None
        })
    );
}

/// Account, row 2: the phone archived and marked Note to Self too: intents deleted, nothing
/// stated.
#[test]
fn three_way_account_done_elsewhere() {
    let w = World::on();
    w.put(10, vec![account_record(1, |_| {})]);
    let own = ConvKey::Direct(aci_user_id(w.f.account.aci).unwrap());
    assert_eq!(w.archive(&own, true, 100), Ok(()));
    assert_eq!(w.mark(&own, true, 101), Ok(()));
    w.events();

    let events = w.merge(
        200,
        vec![account_record(2, |a| {
            a.note_to_self_archived = true;
            a.note_to_self_marked_unread = true;
        })],
    );
    assert!(patch_of(&events, &own).is_none(), "nothing is stated");
    assert!(w.intents().is_empty());
    assert_eq!(
        w.told_own(),
        Some(Roaming {
            archived: true,
            muted_until: None
        })
    );
}

/// Account, row 3: Note to Self is archived (the told base says so), an Archive intent with
/// ours == base (staged as in `three_way_contact_the_phone_wins`; review H2 changed it from an
/// Archive over A1's moved told base), and the phone then unarchived it: the phone wins, stated,
/// and the told base moves.
#[test]
fn three_way_account_the_phone_wins() {
    let w = World::on();
    w.put(
        10,
        vec![account_record(1, |a| a.note_to_self_archived = true)],
    );
    let own_aci = w.f.account.aci;
    let own = ConvKey::Direct(aci_user_id(own_aci).unwrap());
    assert_eq!(w.archive(&own, true, 100), Ok(()));
    let archive = w.intent(ACCOUNT_TARGET, ARCHIVED).unwrap();
    assert_eq!((archive.value, archive.base), (Some(1), Some(1)));
    w.events();

    let events = w.merge(200, vec![account_record(2, |_| {})]);
    assert_eq!(
        patch_of(&events, &own).expect("stated").archived,
        Some(false)
    );
    assert!(w.intents().is_empty());
    assert_eq!(
        w.told_own(),
        Some(Roaming {
            archived: false,
            muted_until: None
        })
    );
}

// ----- pins -----

/// A world with Ada and Bob and an account record that pins Bob.
fn pin_world() -> World {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xada, |_| {}),
            contact(2, 0xb0b, |_| {}),
            account_record(3, |a| a.pinned_conversations = vec![contact_pin(0xb0b)]),
        ],
    );
    w
}

/// Pin, row 1: the user pinned Ada and unpinned Bob; the phone writes its account record again
/// for another reason (still Bob only): both intents stay, and the list the core is told is
/// the record's with OUR removal and OUR addition applied.
#[test]
fn three_way_pin_stays() {
    let w = pin_world();
    assert_eq!(w.pin(&chat(0xada), true, 100), Ok(()));
    assert_eq!(w.pin(&chat(0xb0b), false, 101), Ok(()));
    w.events();

    let events = w.merge(
        200,
        vec![account_record(4, |a| {
            a.pinned_conversations = vec![contact_pin(0xb0b)];
            a.read_receipts = true;
        })],
    );
    assert_eq!(pin_lists(&events), [vec![chat(0xada)]]);
    assert_eq!(w.intents().len(), 2, "both pin intents stay");
}

/// Pin, row 2: the phone pinned Ada and unpinned Bob too: the intents are deleted, the list is
/// the record's.
#[test]
fn three_way_pin_done_elsewhere() {
    let w = pin_world();
    assert_eq!(w.pin(&chat(0xada), true, 100), Ok(()));
    assert_eq!(w.pin(&chat(0xb0b), false, 101), Ok(()));
    w.events();

    let events = w.merge(
        200,
        vec![account_record(4, |a| {
            a.pinned_conversations = vec![contact_pin(0xada)];
        })],
    );
    assert_eq!(pin_lists(&events), [vec![chat(0xada)]]);
    assert!(w.intents().is_empty());
}

/// Pin, row 3: the user pinned Bob, who the record already pins (ours == base), and the phone
/// then unpinned him: the phone wins, the intent is deleted and the list is the record's.
#[test]
fn three_way_pin_the_phone_wins() {
    let w = pin_world();
    assert_eq!(w.pin(&chat(0xb0b), true, 100), Ok(()));
    let pin = w
        .intent(ACCOUNT_TARGET, &pin_field(&person(0xb0b)))
        .unwrap();
    assert_eq!((pin.value, pin.base), (Some(1), Some(1)));
    w.events();

    let events = w.merge(200, vec![account_record(4, |_| {})]);
    assert_eq!(pin_lists(&events), [Vec::<ConvKey>::new()]);
    assert!(w.intents().is_empty());
}

// ----- identity -----

/// A world with Ada's identity key stored (state DEFAULT) and the phone's record of Ada naming it
/// (DEFAULT too); the switch is on, no click yet. Returns the key.
fn identity_world_before_the_click() -> (World, Vec<u8>) {
    let w = World::on();
    let key = libsignal_protocol::IdentityKeyPair::generate(&mut rand::rng())
        .identity_key()
        .serialize()
        .to_vec();
    let sid = ServiceId::from(aci(0xada)).service_id_string();
    w.f.db
        .zone(|tx| -> Result<()> {
            tx.execute(
                "INSERT INTO identities(service_id, identity_key, verified, first_use, updated_at)
                 VALUES (?1, ?2, 0, 0, 1)",
                (&sid, &key[..]),
            )?;
            peers::set_features_hash(tx, &sid, 7, 1)?;
            Ok(())
        })
        .unwrap();
    w.put(
        10,
        vec![contact(1, 0xada, |c| {
            c.identity_key = key.clone();
        })],
    );
    (w, key)
}

/// A world with Ada's identity key stored (state DEFAULT), the phone's record of Ada naming it,
/// and a click that marked her VERIFIED here (the switch is on). Returns the key.
fn identity_world() -> (World, Vec<u8>) {
    let (w, key) = identity_world_before_the_click();
    let sid = ServiceId::from(aci(0xada)).service_id_string();
    // The click: applied here at once (safety.rs), filed as an intent for the phone.
    w.f.db
        .zone(|tx| -> Result<bool> {
            tx.execute(
                "UPDATE identities SET verified = 1 WHERE service_id = ?1",
                [&sid],
            )?;
            intend_identity(tx, &w.f.account, &sid, 1, 0, &key, 100).map(IdentityFiled::is_filed)
        })
        .unwrap()
        .then_some(())
        .expect("filed");
    let intent = w.intent(&person(0xada), IDENTITY).unwrap();
    assert_eq!((intent.value, intent.base), (Some(1), Some(0)));
    w.events();
    (w, key)
}

fn verified_of(w: &World, n: u128) -> i64 {
    let sid = ServiceId::from(aci(n)).service_id_string();
    w.f.db
        .read(|c| -> Result<i64> {
            Ok(c.query_row(
                "SELECT verified FROM identities WHERE service_id = ?1",
                [&sid],
                |r| r.get(0),
            )?)
        })
        .unwrap()
}

fn verified_notices(events: &[BackendEvent]) -> usize {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                BackendEvent::Message(m) if matches!(
                    m.body,
                    kourier_model::Body::System(kourier_model::SystemEvent::VerifiedChanged { .. })
                )
            )
        })
        .count()
}

/// Identity, row 1: the phone's record of Ada still holds the state it had (DEFAULT == base), the
/// click here is newer: the intent stays and the record is NOT applied (no revert, no notice).
#[test]
fn three_way_identity_stays() {
    let (w, key) = identity_world();
    let events = w.merge(
        200,
        vec![contact(2, 0xada, |c| {
            c.identity_key = key.clone();
            c.identity_state = proto::contact_record::IdentityState::Default as i32;
            c.given_name = "A".to_owned();
        })],
    );
    assert!(w.intent(&person(0xada), IDENTITY).is_some(), "it stays");
    assert_eq!(verified_of(&w, 0xada), 1, "our click is not reverted");
    assert_eq!(verified_notices(&events), 0);
}

/// Identity, row 2: the phone marked Ada VERIFIED too (== ours): the intent is deleted, the state
/// is what it is, no notice.
#[test]
fn three_way_identity_done_elsewhere() {
    let (w, key) = identity_world();
    let events = w.merge(
        200,
        vec![contact(2, 0xada, |c| {
            c.identity_key = key.clone();
            c.identity_state = proto::contact_record::IdentityState::Verified as i32;
        })],
    );
    assert!(w.intent(&person(0xada), IDENTITY).is_none(), "closed");
    assert_eq!(verified_of(&w, 0xada), 1);
    assert_eq!(verified_notices(&events), 0);
}

/// Identity, row 3: the phone says UNVERIFIED (neither the base DEFAULT nor our VERIFIED): the
/// phone wins, the intent is deleted and the record's state is applied.
#[test]
fn three_way_identity_the_phone_wins() {
    let (w, key) = identity_world();
    w.merge(
        200,
        vec![contact(2, 0xada, |c| {
            c.identity_key = key.clone();
            c.identity_state = proto::contact_record::IdentityState::Unverified as i32;
        })],
    );
    assert!(w.intent(&person(0xada), IDENTITY).is_none(), "deleted");
    assert_eq!(verified_of(&w, 0xada), 2, "the phone's state is applied");
}

/// Re-check N4: a Clear made here after the phone's Verify was applied from its SYNC stays over the
/// phone's own record of that Verify. The phone verifies K at t1: its Verified sync is applied here
/// at once (local VERIFIED), its storage write is debounced, so the record we hold still says
/// DEFAULT. He clears K here at t2 > t1: the intent's base is the state the key had HERE before
/// the click (VERIFIED), not the stored record's DEFAULT. The phone's record of its Verify then
/// merges: `theirs == base`, "the phone did not touch it after our click": the Clear stays, is not
/// reverted and is written (with the record's DEFAULT as the base it was `theirs != base` and
/// `theirs != ours`, the phone won, and his newer Clear was lost on both devices).
#[test]
fn a_clear_after_the_phones_verify_was_applied_stays_over_the_phones_record_of_it() {
    let (w, key) = identity_world_before_the_click();
    let sid = ServiceId::from(aci(0xada)).service_id_string();
    let user = aci_user_id(aci(0xada)).unwrap();
    let remote_key = libsignal_protocol::IdentityKey::decode(&key).unwrap();

    // The phone's click at 50: its sync is applied here, its record is not merged yet.
    let remote =
        w.f.db
            .zone(|tx| {
                crate::safety::apply_remote_state(
                    tx,
                    &user,
                    &sid,
                    &remote_key,
                    1,
                    60,
                    crate::safety::RemoteSource::Sync { timestamp: 50 },
                )
            })
            .unwrap();
    assert!(matches!(remote, crate::safety::Remote::Applied { .. }));
    assert_eq!(verified_of(&w, 0xada), 1, "the phone's Verify is applied");
    w.events();

    // He clears the key here at 100 (`set_verified` hands the state it had: VERIFIED).
    let filed =
        w.f.db
            .zone(|tx| -> Result<IdentityFiled> {
                tx.execute(
                    "UPDATE identities SET verified = 0 WHERE service_id = ?1",
                    [&sid],
                )?;
                intend_identity(tx, &w.f.account, &sid, 0, 1, &key, 100)
            })
            .unwrap();
    assert_eq!(filed, IdentityFiled::SameKey);
    let intent = w.intent(&person(0xada), IDENTITY).unwrap();
    assert_eq!(
        (intent.value, intent.base),
        (Some(0), Some(1)),
        "the base is the state here before the click, not the record's DEFAULT"
    );
    w.events();

    // The phone's record of its Verify arrives.
    let events = w.merge(
        200,
        vec![contact(2, 0xada, |c| {
            c.identity_key = key.clone();
            c.identity_state = proto::contact_record::IdentityState::Verified as i32;
        })],
    );
    assert!(
        w.intent(&person(0xada), IDENTITY).is_some(),
        "the Clear stays"
    );
    assert_eq!(verified_of(&w, 0xada), 0, "and is not reverted");
    assert_eq!(verified_notices(&events), 0);
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, BackendEvent::ContactUpsert(p) if p.trust.is_some())),
        "the core is not told the phone's older state"
    );

    // And it is written: the record goes out with the state DEFAULT (the field absent) and the key.
    w.set_version(7);
    let (plan, _row) = w.plan_and_row(&w.fetched(7), 210);
    assert!(
        plan.planned
            .iter()
            .any(|p| p.field == IDENTITY && p.value == Some(0)),
        "the Clear is planned"
    );
    let Some(Record::Contact(written)) = decrypted_of(&plan.inserts[0]).record.record else {
        panic!("a contact record is written");
    };
    assert_eq!(written.identity_key, key);
    assert_eq!(
        written.identity_state,
        proto::contact_record::IdentityState::Default as i32,
        "the Clear is written over the phone's VERIFIED"
    );
}

/// Review M1: the phone's state is the state of a KEY. Bob's record holds key K1, VERIFIED; his key
/// changed to K2 (state 2 here) and he Confirms K2 here: the intent's base is NOT K1's VERIFIED
/// (`None`: the phone has no state for K2). Before the click is written the phone learns K2 and
/// he verifies K2 there: the record (K2, VERIFIED) is the phone's own change and wins (with K1's
/// state as the base it would equal the base, "the click is newer", and the older Confirm would
/// revert the phone's Verify). The record that says DEFAULT is the same as the click: done
/// elsewhere.
#[test]
fn a_click_for_a_new_key_does_not_take_the_old_keys_state_as_the_phones() {
    let new_key = || {
        libsignal_protocol::IdentityKeyPair::generate(&mut rand::rng())
            .identity_key()
            .serialize()
            .to_vec()
    };
    let (k1, k2) = (new_key(), new_key());
    let sid = ServiceId::from(aci(0xb0b)).service_id_string();
    let setup = || {
        let w = World::on();
        // Local state: K2 is the stored key and its state is 2 (changed).
        w.f.db
            .zone(|tx| -> Result<()> {
                tx.execute(
                    "INSERT INTO identities(service_id, identity_key, verified, first_use, updated_at)
                     VALUES (?1, ?2, 2, 0, 1)",
                    (&sid, &k2[..]),
                )?;
                peers::set_features_hash(tx, &sid, 7, 1)?;
                Ok(())
            })
            .unwrap();
        // The phone's record still holds K1, VERIFIED.
        w.put(
            10,
            vec![contact(1, 0xb0b, |c| {
                c.identity_key = k1.clone();
                c.identity_state = proto::contact_record::IdentityState::Verified as i32;
            })],
        );
        // Confirm K2 here: DEFAULT, applied at once, filed for the phone.
        w.f.db
            .zone(|tx| -> Result<bool> {
                tx.execute(
                    "UPDATE identities SET verified = 0 WHERE service_id = ?1",
                    [&sid],
                )?;
                intend_identity(tx, &w.f.account, &sid, 0, 2, &k2, 100).map(IdentityFiled::is_filed)
            })
            .unwrap()
            .then_some(())
            .expect("filed");
        w.events();
        w
    };
    let w = setup();
    let intent = w.intent(&person(0xb0b), IDENTITY).unwrap();
    assert_eq!(intent.key.as_deref(), Some(&k2[..]));
    // Review N3 (this assertion changed from `None`): the base is K2's state here before the
    // click (2: a new key after VERIFIED, the phone's rule too), not K1's VERIFIED.
    assert_eq!(
        (intent.value, intent.base),
        (Some(0), Some(2)),
        "K1's VERIFIED is not the phone's state of K2"
    );

    // The phone learns K2 and he verifies it there: the phone's change wins.
    w.merge(
        200,
        vec![contact(2, 0xb0b, |c| {
            c.identity_key = k2.clone();
            c.identity_state = proto::contact_record::IdentityState::Verified as i32;
        })],
    );
    assert!(w.intent(&person(0xb0b), IDENTITY).is_none(), "deleted");
    assert_eq!(verified_of(&w, 0xb0b), 1, "the phone's Verify is applied");

    // The phone says DEFAULT for K2: what he clicked, done elsewhere.
    let w = setup();
    w.merge(
        200,
        vec![contact(2, 0xb0b, |c| {
            c.identity_key = k2.clone();
            c.identity_state = proto::contact_record::IdentityState::Default as i32;
        })],
    );
    assert!(w.intent(&person(0xb0b), IDENTITY).is_none(), "closed");
    assert_eq!(verified_of(&w, 0xb0b), 0);
}

// ----- the rest of the rule -----

/// A record that does not decode runs no rule: the intents stay. A record of a kind this build
/// does not know, a contact record that names nobody, and a contact record whose identity key
/// is no key leave every intent as it was and state nothing. (A record that fails to decrypt or
/// to decode never reaches a merge: `sync` counts it as failed and stores nothing.)
#[test]
fn three_way_a_record_that_does_not_decode_runs_no_rule() {
    let (w, _key) = identity_world();
    w.put(
        12,
        vec![contact(5, 0xb0b, |_| {}), account_record(6, |_| {})],
    );
    let ada = chat(0xada);
    assert_eq!(w.archive(&ada, true, 110), Ok(()));
    assert_eq!(w.pin(&chat(0xb0b), true, 111), Ok(()));
    w.events();
    let before = w.intents();
    assert_eq!(before.len(), 3);

    let unknown = Decrypted {
        key: vec![8; 16],
        kind: 99,
        raw: Zeroizing::new(Vec::new()),
        record: proto::StorageRecord { record: None },
    };
    let nobody = item(
        9,
        ItemType::Contact,
        Record::Contact(proto::ContactRecord::default()),
    );
    let broken_key = contact(7, 0xada, |c| {
        c.identity_key = vec![1, 2, 3];
        c.identity_state = proto::contact_record::IdentityState::Unverified as i32;
    });
    let events = w.merge(200, vec![unknown, nobody, broken_key]);
    assert_eq!(w.intents(), before, "every intent is as it was");
    assert_eq!(verified_of(&w, 0xada), 1);
    assert_eq!(verified_notices(&events), 0);
}

/// H3 (the move itself is B6's landing; the rule READS `base`): after the write that carried
/// Archive landed, `base` of the newer click (Unarchive) is the written value, so the phone's
/// record that holds what we wrote (`theirs == base`) leaves that click alone. With the base
/// still at the old value the same record would look like the phone's own change and win.
#[test]
fn base_moves_to_the_written_value() {
    let w = World::on();
    w.put(
        10,
        vec![contact(1, 0xada, |_| {}), account_record(2, |_| {})],
    );
    let ada = chat(0xada);
    assert_eq!(w.archive(&ada, true, 100), Ok(()));
    let carried = w.intent(&person(0xada), ARCHIVED).unwrap();
    // The write in flight carries Archive; during the PUT the user clicks Unarchive.
    w.in_flight(&format!(
        r#"[{{"target": "{}", "field": "archived", "value": 1, "seq": {}}}]"#,
        person(0xada),
        carried.seq
    ));
    assert_eq!(w.archive(&ada, false, 200), Ok(()));
    let newer = w.intent(&person(0xada), ARCHIVED).unwrap();
    assert_eq!(
        (newer.value, newer.base),
        (Some(0), Some(1)),
        "base = written"
    );
    // The landing (B6): the row is gone, the click stays with its moved base.
    w.f.db
        .zone(|tx| -> Result<()> {
            tx.execute("DELETE FROM storage_write_inflight", [])?;
            Ok(())
        })
        .unwrap();
    w.events();

    // The fetched record is what we wrote: the click survives, nothing is stated.
    let events = w.merge(300, vec![contact(3, 0xada, |c| c.archived = true)]);
    assert!(w.intent(&person(0xada), ARCHIVED).is_some(), "stays");
    assert!(patch_of(&events, &ada).is_none());

    // The same record with the base NOT moved: the phone seems to have archived on its own.
    w.f.db
        .zone(|tx| -> Result<()> {
            tx.execute("UPDATE storage_intents SET base = 0", [])?;
            Ok(())
        })
        .unwrap();
    let events = w.merge(400, vec![contact(4, 0xada, |c| c.archived = true)]);
    assert!(w.intent(&person(0xada), ARCHIVED).is_none(), "overruled");
    assert_eq!(
        patch_of(&events, &ada).expect("stated").archived,
        Some(true)
    );
}

/// The core's own clearing of a mark follows the same table: the phone cleared the mark itself
/// (done elsewhere: the intent goes and nothing is written), or the record is merely rotated
/// and still holds the mark we are clearing (the clear stays and is not stated back).
#[test]
fn three_way_the_automatic_clear_follows_the_table() {
    for phone_cleared in [false, true] {
        let w = World::on();
        w.put(10, vec![contact(1, 0xada, |c| c.marked_unread = true)]);
        let ada = chat(0xada);
        assert_eq!(w.automatic(&ada, 20, 30), Automatic::Filed);
        let clear = w.intent(&person(0xada), MARKED_UNREAD).unwrap();
        assert!(clear.automatic);
        assert_eq!((clear.value, clear.base), (Some(0), Some(1)));
        w.events();
        let events = w.merge(
            200,
            vec![contact(2, 0xada, |c| {
                c.marked_unread = !phone_cleared;
                c.blocked = true;
            })],
        );
        let patch = patch_of(&events, &ada).expect("the block is stated");
        assert_eq!(patch.marked_unread, None, "the core shows the clear");
        assert_eq!(
            w.intent(&person(0xada), MARKED_UNREAD).is_some(),
            !phone_cleared,
            "stays unless the phone cleared it too"
        );
    }
}

/// B3's open item (d): an automatic clear that is still queued must not erase a mark the phone
/// made AFTER the next reading was asked. The merge cannot retire it (a record that still says
/// marked is `theirs == base`), so the reading that finds the newer mark does: the older clear
/// goes, the chat is marked again for the core. A click of the user's own (Mark as Read) stays.
#[test]
fn three_way_a_newer_phone_mark_retires_the_older_automatic_clear() {
    let w = World::on();
    w.put(10, vec![contact(1, 0xada, |c| c.marked_unread = true)]);
    let ada = chat(0xada);
    assert_eq!(w.automatic(&ada, 20, 30), Automatic::Filed);
    // The phone marks the chat again (the record is the same in every flag) at 100.
    w.merge(100, vec![contact(2, 0xada, |c| c.marked_unread = true)]);
    assert!(
        w.intent(&person(0xada), MARKED_UNREAD).is_some(),
        "the merge leaves it: theirs == base"
    );
    // The user reads the chat again, at 50 (before the phone's mark was merged).
    assert_eq!(w.automatic(&ada, 50, 120), Automatic::PhoneKept);
    assert!(
        w.intent(&person(0xada), MARKED_UNREAD).is_none(),
        "the older clear is retired"
    );
    assert_eq!(
        patch_of(&w.events(), &ada).expect("told").marked_unread,
        Some(true)
    );

    // A click is not touched, and it RULES (review L2): Mark as Read on a chat the phone marked
    // after the reading. The click is newer than the phone's mark and will unmark the phone, so
    // the reading says nothing of the phone's mark (it used to be `PhoneKept`: the core marked
    // again while the click was about to unmark the phone).
    let g = group_chat(0x31);
    w.put(
        200,
        vec![
            group_record(3, 0x31, |r| r.marked_unread = true),
            account_record(4, |_| {}),
        ],
    );
    assert_eq!(w.mark(&g, false, 210), Ok(()));
    w.events();
    assert_eq!(w.automatic(&g, 150, 220), Automatic::Dropped);
    assert!(w.intent(&group_t(0x31), MARKED_UNREAD).is_some());
    assert!(
        patch_of(&w.events(), &g).is_none_or(|p| p.marked_unread.is_none()),
        "the core is not told the phone's mark"
    );
}

/// The pin list restated at a group's first fetch (`storage::pins_with_new_group`) is not a merge
/// and runs no rule, but it must not undo a pin change that has not reached the phone: the group
/// the stored record pins is no longer named while an unpin of it is queued.
#[test]
fn three_way_a_group_fetched_later_keeps_the_live_pins() {
    let w = World::on();
    w.put(
        10,
        vec![
            group_record(1, 0x31, |_| {}),
            account_record(2, |a| a.pinned_conversations = vec![group_pin(0x31)]),
        ],
    );
    w.core_has_group(0x31);
    let keys = groups::GroupKeys::from_master([0x31; 32]);
    let list = || {
        w.f.db
            .read(|c| crate::storage::pins_with_new_group(c, &keys))
            .unwrap()
    };
    assert!(
        matches!(list(), Some(BackendEvent::PinnedConversations(l)) if l == [group_chat(0x31)]),
        "the stored record pins the group"
    );
    assert_eq!(w.pin(&group_chat(0x31), false, 100), Ok(()));
    assert!(list().is_none(), "the queued unpin is in force");
}

/// What a merge does with a settled field: an open field is stated when the old diff says it
/// changed, a field the phone won is ALWAYS stated (an explicit upsert, whatever the told base
/// says), a held field never; the mirror keeps ours only for a held field.
#[test]
fn three_way_a_phone_win_is_always_stated_and_a_held_field_never() {
    for differs in [false, true] {
        assert_eq!(Settled::Open.states(differs), differs);
        assert!(Settled::Theirs.states(differs), "the phone wins: stated");
        assert!(!Settled::Held(Some(1)).states(differs), "held: not stated");
        assert!(!Settled::Held(None).states(differs));
    }
    for (settled, theirs, kept) in [
        (Settled::Held(Some(1)), false, true),
        (Settled::Held(Some(0)), true, false),
        (Settled::Open, true, true),
        (Settled::Open, false, false),
        (Settled::Theirs, true, true),
        (Settled::Theirs, false, false),
    ] {
        assert_eq!(settled.flag(theirs), kept, "{settled:?} over {theirs}");
    }
    for (settled, theirs, kept) in [
        (Settled::Held(Some(OURS)), None, Some(OURS)),
        (Settled::Held(None), Some(THEIRS), None),
        (Settled::Open, Some(THEIRS), Some(THEIRS)),
        (Settled::Theirs, None, None),
    ] {
        assert_eq!(settled.mute(theirs), kept, "{settled:?} over {theirs:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Landing (B6; §3.6, critic X3, X4, Y5, Y6, M4) and the rebuilt core.db
// ---------------------------------------------------------------------------------------------

fn carried(target: &str, field: &str, value: Option<i64>, seq: i64) -> Planned {
    Planned {
        target: target.to_owned(),
        field: field.to_owned(),
        value,
        seq,
        rides: false,
        overtaken: false,
        prior: None,
        state_at_landing: false,
    }
}

fn key(n: u8) -> Vec<u8> {
    vec![n; 16]
}

impl World {
    /// The in-flight row of a write that sets `version`, inserts the identifiers `keys` and
    /// carries `planned`, planned under generation `generation`.
    fn row(&self, version: u64, keys: &[u8], planned: Vec<Planned>, generation: i64) -> InFlight {
        let row = InFlight {
            version,
            new_keys: keys.iter().map(|&n| key(n)).collect(),
            planned,
            generation,
            planned_at: 100,
        };
        self.f
            .db
            .zone(|tx| write::write_in_flight(tx, &row))
            .unwrap();
        row
    }

    fn land(&self, row: &InFlight, now: i64) -> Landing {
        self.f
            .db
            .zone(|tx| land(tx, &self.f.account, row, now, &self.wake))
            .unwrap()
    }

    fn resolve(&self, fetched: &proto::ManifestRecord, now: i64) -> Resolved {
        self.f
            .db
            .zone(|tx| resolve_in_flight(tx, &self.f.account, fetched, now, &self.wake))
            .unwrap()
    }

    fn generation(&self) -> i64 {
        self.f.db.read(storage_generation).unwrap()
    }

    /// Overwrites the live intent as a click would have filed it BEFORE the in-flight row existed
    /// (the plan was made, the click came, the row was written): newer than the carried one, with
    /// the base of the stored record and not the written value.
    fn gap_click(
        &self,
        target: &str,
        field: &str,
        value: Option<i64>,
        base: Option<i64>,
        shown: Option<i64>,
        seq: i64,
    ) {
        self.f
            .db
            .zone(|tx| -> Result<()> {
                tx.execute(
                    "UPDATE storage_intents SET value = ?3, base = ?4, shown = ?5, seq = ?6
                     WHERE target = ?1 AND field = ?2",
                    (target, field, value, base, shown, seq),
                )?;
                Ok(())
            })
            .unwrap();
    }
}

/// A manifest at `version` that lists `keys`.
fn manifest_listing(version: u64, keys: &[u8]) -> proto::ManifestRecord {
    proto::ManifestRecord {
        version,
        source_device: 1,
        identifiers: keys
            .iter()
            .map(|&n| proto::manifest_record::Identifier {
                raw: key(n),
                r#type: ItemType::Contact as i32,
            })
            .collect(),
        record_ikm: vec![9; 32],
    }
}

/// Y5 and H3: the landing zone deletes EXACTLY the intents whose `seq` is the carried one; an
/// intent a newer click put on a carried field stays with `base` = the written value; an intent
/// the write did not carry is not touched; no record is stored; the row is deleted; with the
/// switch on the core is told nothing (it shows ours).
#[test]
fn the_landing_closes_exactly_the_carried_intents() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xada, |_| {}),
            contact(2, 0xb0b, |_| {}),
            contact(3, 0xc1a, |_| {}),
            group_record(4, 0x42, |_| {}),
            account_record(5, |_| {}),
        ],
    );
    assert_eq!(w.archive(&chat(0xada), true, 100), Ok(()));
    assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 101), Ok(()));
    assert_eq!(w.mark(&group_chat(0x42), true, 102), Ok(()));
    let at = |t: &str, f: &str| w.intent(t, f).unwrap();
    let (ada, bob, grp) = (
        at(&person(0xada), ARCHIVED),
        at(&person(0xb0b), MUTED_UNTIL),
        at(&group_t(0x42), MARKED_UNREAD),
    );
    let row = w.row(
        8,
        &[0xa1, 0xa2, 0xa3],
        vec![
            carried(&person(0xada), ARCHIVED, Some(1), ada.seq),
            carried(&person(0xb0b), MUTED_UNTIL, Some(9_000), bob.seq),
            carried(&group_t(0x42), MARKED_UNREAD, Some(1), grp.seq),
        ],
        0,
    );
    // Bob's mute is changed again, a click filed after the plan and before the row existed (a
    // newer click on a carried field with the stored record's base); Chiara is archived (a field
    // the write did not carry).
    w.gap_click(
        &person(0xb0b),
        MUTED_UNTIL,
        Some(50_000),
        None,
        None,
        bob.seq + 1,
    );
    assert_eq!(w.archive(&chat(0xc1a), true, 151), Ok(()));
    let chiara = at(&person(0xc1a), ARCHIVED);
    w.events();
    let rows_before = w.f.db.read(|c| count(c, "storage_records")).unwrap();

    assert_eq!(
        w.land(&row, 200),
        Landing::Landed {
            closed: 2,
            moved: 1
        }
    );
    assert!(w.intent(&person(0xada), ARCHIVED).is_none(), "closed");
    assert!(w.intent(&group_t(0x42), MARKED_UNREAD).is_none(), "closed");
    let bob_now = at(&person(0xb0b), MUTED_UNTIL);
    assert_eq!(bob_now.value, Some(50_000), "the newer click stays");
    assert!(bob_now.seq > bob.seq);
    assert_eq!(bob_now.base, Some(9_000), "its base is the written value");
    assert_eq!(
        at(&person(0xc1a), ARCHIVED),
        chiara,
        "not carried: untouched"
    );
    assert_eq!(w.in_flight_rows(), 0, "the row is deleted");
    assert_eq!(
        w.f.db.read(|c| count(c, "storage_records")).unwrap(),
        rows_before,
        "the landing stores nothing of the records"
    );
    assert!(
        w.events().is_empty(),
        "the switch is on: the core shows ours"
    );
    // Landing again (the same row in hand) closes nothing more; the newer click keeps the same
    // base.
    assert_eq!(
        w.land(&row, 201),
        Landing::Landed {
            closed: 0,
            moved: 1
        }
    );
    assert_eq!(at(&person(0xb0b), MUTED_UNTIL), bob_now);
}

fn count(conn: &Connection, table: &str) -> Result<i64> {
    Ok(conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
}

/// X4: after a kill the landing found from the in-flight row runs BEFORE the merge of the
/// manifest that lists our own records. A click filed after the plan (so with the stored record's
/// base) on a carried field survives that merge only because the landing moved its base to the
/// written value; merged first, our own record looks like the phone's change and wins.
#[test]
fn the_landing_moves_base_before_the_merge() {
    // The user archived Ada (seq n); the write carrying it reached the server and the app was
    // killed before its 200 zone; before the kill he had already clicked Unarchive.
    let staged = || {
        let w = World::on();
        w.put(
            10,
            vec![contact(1, 0xada, |_| {}), account_record(2, |_| {})],
        );
        assert_eq!(w.archive(&chat(0xada), true, 100), Ok(()));
        let archive = w.intent(&person(0xada), ARCHIVED).unwrap();
        let row = w.row(
            8,
            &[0xa1],
            vec![carried(&person(0xada), ARCHIVED, Some(1), archive.seq)],
            0,
        );
        w.gap_click(
            &person(0xada),
            ARCHIVED,
            Some(0),
            Some(0),
            Some(1),
            archive.seq + 1,
        );
        w.events();
        (w, row)
    };
    let our_own_record = || vec![contact(0xa1, 0xada, |c| c.archived = true)];

    // The order of the job: the landing, then the merge of the fetched manifest.
    let (w, _row) = staged();
    let resolved = w.resolve(&manifest_listing(8, &[0xa1]), 300);
    assert_eq!(
        resolved,
        Resolved::Found {
            landing: Landing::Landed {
                closed: 0,
                moved: 1
            },
            assumed: false
        }
    );
    let after = w.intent(&person(0xada), ARCHIVED).expect("the newer click");
    assert_eq!((after.value, after.base), (Some(0), Some(1)));
    let events = w.merge(310, our_own_record());
    let kept = w
        .intent(&person(0xada), ARCHIVED)
        .expect("survives the merge");
    assert_eq!(kept.value, Some(0));
    assert!(
        patch_of(&events, &chat(0xada)).is_none_or(|p| p.archived.is_none()),
        "nothing is stated: the core shows his Unarchive"
    );

    // The other order (what a sync that merges first would do): our own record is the phone's
    // word, the click is overruled and the core is told "archived".
    let (w, row) = staged();
    let events = w.merge(310, our_own_record());
    assert!(
        w.intent(&person(0xada), ARCHIVED).is_none(),
        "merged first: the click is lost"
    );
    assert_eq!(
        patch_of(&events, &chat(0xada)).expect("stated").archived,
        Some(true)
    );
    let _ = row;
}

/// §3.6: a write found in flight is judged by the FETCHED manifest. Not landed: the row goes and
/// every intent is as it was. Landed: the zone runs. A manifest that moved on and lists none of
/// our ids counts as landed (BQ9) and says so.
#[test]
fn a_found_write_is_judged_by_the_fetched_manifest() {
    let setup = || {
        let w = World::on();
        w.put(
            10,
            vec![contact(1, 0xada, |_| {}), account_record(2, |_| {})],
        );
        assert_eq!(w.archive(&chat(0xada), true, 100), Ok(()));
        let intent = w.intent(&person(0xada), ARCHIVED).unwrap();
        let row = w.row(
            8,
            &[0xa1],
            vec![carried(&person(0xada), ARCHIVED, Some(1), intent.seq)],
            0,
        );
        (w, intent, row)
    };

    // No row: nothing happens.
    let w = World::on();
    assert_eq!(
        w.resolve(&manifest_listing(8, &[0xa1]), 300),
        Resolved::NoRow
    );

    // The server is behind the write, or the phone wrote the version we meant to write: the row
    // is deleted, the intent is untouched and goes again.
    for fetched in [manifest_listing(7, &[0xa1]), manifest_listing(8, &[0x01])] {
        let (w, intent, _) = setup();
        assert_eq!(w.resolve(&fetched, 300), Resolved::NotLanded);
        assert_eq!(w.in_flight_rows(), 0);
        assert_eq!(w.intent(&person(0xada), ARCHIVED), Some(intent));
    }

    // Landed: one of our ids at the write's version, or at a later one.
    for fetched in [manifest_listing(8, &[0xa1]), manifest_listing(12, &[0xa1])] {
        let (w, ..) = setup();
        assert_eq!(
            w.resolve(&fetched, 300),
            Resolved::Found {
                landing: Landing::Landed {
                    closed: 1,
                    moved: 0
                },
                assumed: false
            }
        );
        assert!(w.intents().is_empty());
        assert_eq!(w.in_flight_rows(), 0);
    }

    // The manifest moved on, lists none of our ids but the record our write DELETED is still
    // listed: the write was not applied (D1), the row is deleted and the intent goes again.
    let (w, intent, _) = setup();
    assert_eq!(
        w.resolve(&manifest_listing(12, &[0x01]), 300),
        Resolved::NotLanded
    );
    assert_eq!(w.in_flight_rows(), 0);
    assert_eq!(w.intent(&person(0xada), ARCHIVED), Some(intent));

    // The phone wrote again and replaced our records (the deleted one is gone too): counted as
    // landed.
    let (w, ..) = setup();
    assert_eq!(
        w.resolve(&manifest_listing(12, &[0x02]), 300),
        Resolved::Found {
            landing: Landing::Landed {
                closed: 1,
                moved: 0
            },
            assumed: true
        }
    );
    assert!(w.intents().is_empty(), "a lost click, not an undone phone");
    assert_eq!(w.in_flight_rows(), 0);
}

/// Y6: the switch went off while the PUT was on its way. `drop_all` kept the carried intents and
/// stated nothing for a newer click on a carried field; the write lands: the landing still
/// records it (the intents are closed, the row deleted) and tells the core what the phone now
/// has, the pin list included, and the told bases follow.
#[test]
fn a_landing_with_the_switch_off_states_the_written_values() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xada, |_| {}),
            contact(2, 0xb0b, |_| {}),
            group_record(3, 0x42, |_| {}),
            account_record(4, |a| a.pinned_conversations = vec![contact_pin(0xb0b)]),
        ],
    );
    let g = group_chat(0x42);
    assert_eq!(w.mark(&g, true, 100), Ok(()));
    assert_eq!(w.archive(&g, true, 101), Ok(()));
    assert_eq!(w.pin(&chat(0xada), true, 102), Ok(()));
    assert_eq!(w.mute(&chat(0xb0b), Some(7_000), 103), Ok(()));
    let at = |t: &str, f: &str| w.intent(t, f).unwrap();
    let row = w.row(
        8,
        &[0xa1, 0xa2],
        vec![
            carried(
                &group_t(0x42),
                MARKED_UNREAD,
                Some(1),
                at(&group_t(0x42), MARKED_UNREAD).seq,
            ),
            carried(
                &group_t(0x42),
                ARCHIVED,
                Some(1),
                at(&group_t(0x42), ARCHIVED).seq,
            ),
            carried(
                ACCOUNT_TARGET,
                &pin_field(&person(0xada)),
                Some(1),
                at(ACCOUNT_TARGET, &pin_field(&person(0xada))).seq,
            ),
        ],
        0,
    );
    // During the PUT he unarchives the group and unpins Ada again; Bob's mute was not carried.
    assert_eq!(w.archive(&g, false, 104), Ok(()));
    assert_eq!(w.pin(&chat(0xada), false, 105), Ok(()));
    w.events();

    w.switch(false);
    let dropped =
        w.f.db
            .zone(|tx| drop_all(tx, &w.f.account, 200, &w.wake))
            .unwrap();
    assert_eq!(dropped, 3, "Bob's mute and the two newer clicks");
    w.events();
    assert_eq!(w.intents().len(), 1, "only the carried mark stays");

    // The write lands (the 200 zone, or the landing found at the next start).
    assert_eq!(
        w.land(&row, 300),
        Landing::Landed {
            closed: 1,
            moved: 0
        }
    );
    assert!(
        w.intents().is_empty(),
        "recorded: every carried intent closed"
    );
    assert_eq!(w.in_flight_rows(), 0);
    let events = w.events();
    let grp = patch_of(&events, &g).expect("the group's written values are stated");
    assert_eq!(grp.marked_unread, Some(true));
    assert_eq!(
        grp.archived,
        Some(true),
        "what the phone has, not his later click"
    );
    assert_eq!(
        pin_lists(&events),
        [vec![chat(0xb0b), chat(0xada)]],
        "the pin list the phone now has: the record's, Ada added last"
    );
    assert!(
        patch_of(&events, &chat(0xb0b)).is_none(),
        "Bob's mute was not in the write: it was restated by drop_all, not now"
    );
    assert_eq!(
        w.group_mirror(0x42),
        (true, true, None),
        "the told base follows"
    );
}

/// M4: `reset` (a new storage key, a rebuilt core.db) bumps `storage_generation` and drops the
/// in-flight row, and keeps the intents. A landing zone planned before it (the 200 path of a job
/// that was awaiting its PUT) drops its result: it closes nothing and states nothing.
#[test]
fn a_reset_drops_the_landing() {
    let w = World::on();
    w.put(
        10,
        vec![contact(1, 0xada, |_| {}), account_record(2, |_| {})],
    );
    assert_eq!(w.archive(&chat(0xada), true, 100), Ok(()));
    let intent = w.intent(&person(0xada), ARCHIVED).unwrap();
    let row = w.row(
        8,
        &[0xa1],
        vec![carried(&person(0xada), ARCHIVED, Some(1), intent.seq)],
        w.generation(),
    );
    assert_eq!(w.generation(), 0);
    w.events();

    w.f.db.zone(|tx| crate::storage::reset(tx, 500)).unwrap();
    assert_eq!(w.generation(), 1);
    assert_eq!(w.in_flight_rows(), 0, "the row goes with the records");
    assert_eq!(w.f.db.read(|c| count(c, "storage_records")).unwrap(), 0);
    assert_eq!(
        w.intent(&person(0xada), ARCHIVED),
        Some(intent.clone()),
        "kept"
    );
    w.events();

    // The job that was awaiting its PUT gets its 200: the result is dropped.
    assert_eq!(w.land(&row, 600), Landing::Stale);
    assert_eq!(
        w.intent(&person(0xada), ARCHIVED),
        Some(intent.clone()),
        "still kept"
    );
    assert!(w.events().is_empty());
    // A landing found at the next start has no row to find.
    assert_eq!(
        w.resolve(&manifest_listing(8, &[0xa1]), 700),
        Resolved::NoRow
    );

    // A write planned after the reset lands as usual.
    let fresh = w.row(
        9,
        &[0xb1],
        vec![carried(&person(0xada), ARCHIVED, Some(1), intent.seq)],
        w.generation(),
    );
    assert_eq!(
        w.land(&fresh, 800),
        Landing::Landed {
            closed: 1,
            moved: 0
        }
    );
    assert!(w.intents().is_empty());
    // A second reset moves the generation again.
    w.f.db.zone(|tx| crate::storage::reset(tx, 900)).unwrap();
    assert_eq!(w.generation(), 2);
}

/// Review M2, a write that did not land: Mute Bob 1 h was planned (the PUT is on its way), then
/// the switch went off. `drop_all` keeps the carried intent and the row (Y6: the landing will close
/// it). The write never lands (the server is behind it): the row is deleted, nothing is carried
/// any more and nothing will ever write the intent, so it is dropped and the stored value
/// restated. With the switch ON the intent stays and is planned again.
#[test]
fn a_write_that_did_not_land_with_the_switch_off_drops_what_it_carried() {
    let setup = |switch_off: bool| {
        let w = World::on();
        w.put(
            10,
            vec![contact(1, 0xb0b, |_| {}), account_record(2, |_| {})],
        );
        assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
        let mute = w.intent(&person(0xb0b), MUTED_UNTIL).unwrap();
        w.row(
            8,
            &[0xa1],
            vec![carried(&person(0xb0b), MUTED_UNTIL, Some(9_000), mute.seq)],
            0,
        );
        w.events();
        if switch_off {
            w.switch(false);
            let dropped =
                w.f.db
                    .zone(|tx| drop_all(tx, &w.f.account, 200, &w.wake))
                    .unwrap();
            assert_eq!(
                dropped, 0,
                "the carried intent stays while the write is on its way"
            );
            assert!(w.events().is_empty());
        }
        w
    };

    let w = setup(true);
    assert_eq!(
        w.resolve(&manifest_listing(7, &[0xa1]), 300),
        Resolved::NotLanded
    );
    assert!(w.intents().is_empty(), "no intent is left");
    assert_eq!(w.in_flight_rows(), 0);
    let bob = patch_of(&w.events(), &chat(0xb0b)).expect("the stored value is restated");
    assert_eq!(bob.muted_until, Patch::Clear);
    assert_eq!(w.told(0xb0b).unwrap().muted_until, None);

    // The control: the switch is on, the intent is planned again.
    let w = setup(false);
    assert_eq!(
        w.resolve(&manifest_listing(7, &[0xa1]), 300),
        Resolved::NotLanded
    );
    assert_eq!(w.intents().len(), 1, "kept");
    assert!(w.events().is_empty());
}

/// Review M2, the stale form: a `storage::reset` ran during the PUT and the switch is off. The
/// result of the write is dropped (`Landing::Stale`) and nothing carries the intent any more.
#[test]
fn a_stale_landing_with_the_switch_off_drops_what_it_carried() {
    let w = World::on();
    w.put(
        10,
        vec![contact(1, 0xb0b, |_| {}), account_record(2, |_| {})],
    );
    assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
    let mute = w.intent(&person(0xb0b), MUTED_UNTIL).unwrap();
    let row = w.row(
        8,
        &[0xa1],
        vec![carried(&person(0xb0b), MUTED_UNTIL, Some(9_000), mute.seq)],
        w.generation(),
    );
    w.switch(false);
    w.f.db
        .zone(|tx| drop_all(tx, &w.f.account, 200, &w.wake))
        .unwrap();
    w.f.db.zone(|tx| crate::storage::reset(tx, 500)).unwrap();
    assert_eq!(w.intents().len(), 1, "the carried intent survived both");
    w.events();

    assert_eq!(w.land(&row, 600), Landing::Stale);
    assert!(w.intents().is_empty(), "no intent is left");
    let bob = patch_of(&w.events(), &chat(0xb0b)).expect("the value is restated");
    assert_eq!(bob.muted_until, Patch::Clear);
}

impl World {
    /// An in-flight row this build cannot read: `planned` is JSON (the table checks that) of a
    /// shape `Planned` does not read, or `new_keys` is not a whole number of identifiers.
    fn unreadable_row(&self, planned: &str, new_keys: &[u8]) {
        self.f
            .db
            .zone(|tx| -> Result<()> {
                tx.execute(
                    "INSERT INTO storage_write_inflight(id, version, new_keys, planned, generation,
                       planned_at) VALUES (1, 8, ?2, ?1, 0, 100)",
                    (planned, new_keys),
                )?;
                Ok(())
            })
            .unwrap();
    }
}

/// Review L3: a row this build cannot read is a write that did not land. Every reader goes on
/// (one warn line, no content): the click files, the receive zone's hook returns, `drop_all`
/// returns and the sync's landing judgement says `NotLanded`; each deletes the row and none
/// returns an `Error::Decode`. The intents stay and are planned again.
#[test]
fn an_unreadable_in_flight_row_is_a_write_that_did_not_land() {
    for (planned, new_keys) in [(r#"[{"target": "x"}]"#, &[][..]), ("[]", &[0xAA][..])] {
        let setup = || {
            let w = World::on();
            w.put(
                10,
                vec![
                    contact(1, 0xada, |_| {}),
                    contact(2, 0xb0b, |c| c.marked_unread = true),
                    account_record(3, |_| {}),
                ],
            );
            assert_eq!(w.archive(&chat(0xada), true, 100), Ok(()));
            w.events();
            w.unreadable_row(planned, new_keys);
            assert_eq!(w.in_flight_rows(), 1);
            w
        };

        // A click.
        let w = setup();
        assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 120), Ok(()));
        assert_eq!(w.in_flight_rows(), 0, "deleted");
        assert_eq!(w.intents().len(), 2);

        // A reading (the core's durable clear).
        let w = setup();
        assert_eq!(w.automatic(&chat(0xb0b), 90, 130), Automatic::Filed);
        assert_eq!(w.in_flight_rows(), 0);

        // A message in a chat with a live Archive (the receive zone).
        let w = setup();
        w.f.db
            .zone(|tx| chat_left_the_archive(tx, &person(0xada), 140))
            .unwrap();
        assert_eq!(w.in_flight_rows(), 0);
        assert!(
            w.intent(&person(0xada), ARCHIVED).is_none(),
            "nothing in flight archives the chat: the live Archive is overtaken"
        );

        // The switch goes off.
        let w = setup();
        w.switch(false);
        let dropped =
            w.f.db
                .zone(|tx| drop_all(tx, &w.f.account, 150, &w.wake))
                .unwrap();
        assert_eq!(dropped, 1, "nothing is carried: the Archive is dropped");
        assert_eq!(w.in_flight_rows(), 0);

        // The landing judgement of a fetched manifest.
        let w = setup();
        assert_eq!(
            w.resolve(&manifest_listing(8, &[0xa1]), 160),
            Resolved::NotLanded
        );
        assert_eq!(w.in_flight_rows(), 0);
        assert_eq!(w.intents().len(), 1, "kept, to be planned again");
    }
}

/// Review L3, through the real `storage::sync`: the Storage Service sync after a start with an
/// unreadable in-flight row goes on and merges what it fetched; the row is gone and the click
/// stays.
#[tokio::test]
async fn a_sync_with_an_unreadable_in_flight_row_goes_on() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = World::on();
            w.put(
                10,
                vec![contact(1, 0xada, |_| {}), account_record(2, |_| {})],
            );
            w.set_version(7);
            assert_eq!(w.archive(&chat(0xada), true, 100), Ok(()));
            w.unreadable_row(r#"[{"target": "x"}]"#, &[]);
            let fresh = contact(0xb1, 0xb0b, |_| {});
            let server = server_listing(
                8,
                &[
                    (key(1), ItemType::Contact),
                    (key(2), ItemType::Account),
                    (fresh.key.clone(), ItemType::Contact),
                ],
                &[&fresh],
            );

            sync_over(&w, server, None).await;

            assert_eq!(w.in_flight_rows(), 0, "the unreadable row is gone");
            assert_eq!(w.version(), Some(8), "the fetched manifest was merged");
            assert!(
                w.intent(&person(0xada), ARCHIVED).is_some(),
                "the click stays"
            );
        })
        .await;
}

/// A rebuilt core.db has none of what was changed here and not yet written, and the merges leave
/// a field an intent holds unstated: `rebuild_events` states the intents' values (the features
/// of a chat the core has not got in front), moves the told bases, and the account record merged
/// again by the full sync states the pin list with the live pin intents applied.
#[test]
fn rebuild_restates_live_intents() {
    let w = World::on();
    let records = || {
        vec![
            contact(1, 0xada, |_| {}),
            contact(2, 0xb0b, |_| {}),
            contact(5, 0xc1a, |_| {}),
            group_record(3, 0x42, |_| {}),
            account_record(4, |a| a.pinned_conversations = vec![contact_pin(0xb0b)]),
        ]
    };
    w.put(10, records());
    let g = group_chat(0x42);
    assert_eq!(w.archive(&chat(0xada), true, 100), Ok(()));
    assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 101), Ok(()));
    assert_eq!(w.mark(&g, true, 102), Ok(()));
    assert_eq!(w.pin(&chat(0xb0b), false, 104), Ok(()));
    assert_eq!(w.pin(&chat(0xc1a), true, 105), Ok(()));
    let intents_before = w.intents();
    assert_eq!(intents_before.len(), 5);
    w.events();
    let blobs = tempfile::tempdir().unwrap();

    crate::maintenance::rebuild_events(&w.f.db, &w.f.account, blobs.path(), 600, &w.wake).unwrap();
    let events = w.events();
    let names: Vec<&str> = events.iter().map(BackendEvent::name).collect();
    assert_eq!(&names[..2], ["AccountReady", "AccountCaps"]);
    let ada = patch_of(&events, &chat(0xada)).expect("Ada's archive is restated");
    assert_eq!(ada.archived, Some(true));
    assert_eq!(
        ada.muted_until,
        Patch::Unchanged,
        "only what the intent holds"
    );
    let bob = patch_of(&events, &chat(0xb0b)).expect("Bob's mute is restated");
    assert_eq!(bob.muted_until, Patch::Set(9_000));
    assert_eq!(bob.archived, None);
    let grp = patch_of(&events, &g).expect("the group's mark is restated");
    assert_eq!(grp.marked_unread, Some(true));
    // The features of a 1:1 chat the new core has not got come in front of its flags.
    let features_at = events.iter().position(|e| {
        matches!(e, BackendEvent::ConversationUpsert(p) if p.key == chat(0xada) && p.features.is_some())
    });
    let flags_at = events.iter().position(|e| {
        matches!(e, BackendEvent::ConversationUpsert(p) if p.key == chat(0xada) && p.archived.is_some())
    });
    assert!(features_at.is_some() && features_at < flags_at, "{names:?}");
    // The told bases follow what the new core was told.
    assert_eq!(w.told(0xada).map(|r| r.archived), Some(true));
    assert_eq!(w.told(0xb0b).map(|r| r.muted_until), Some(Some(9_000)));
    assert_eq!(w.group_mirror(0x42), (false, true, None));
    // The intents are kept as they were; the records and the version went (reset).
    assert_eq!(w.intents(), intents_before);
    assert_eq!(w.f.db.read(|c| count(c, "storage_records")).unwrap(), 0);
    assert_eq!(w.generation(), 1);

    // The full sync: the phone's records again. The merge leaves every held field alone (the
    // core shows ours since the rebuild) and states the pin list with our pins applied.
    let events = w.merge(700, records());
    assert!(
        patch_of(&events, &chat(0xada)).is_none_or(|p| p.archived.is_none()),
        "Ada's archive is held: not stated again"
    );
    assert!(
        patch_of(&events, &chat(0xb0b)).is_none_or(|p| p.muted_until == Patch::Unchanged),
        "Bob's mute is held: not stated again"
    );
    assert!(
        patch_of(&events, &g).is_none_or(|p| p.marked_unread.is_none()),
        "the group's mark is held: not stated again"
    );
    assert_eq!(
        pin_lists(&events),
        [vec![chat(0xc1a)]],
        "Bob unpinned, Chiara pinned"
    );
    assert_eq!(w.intents(), intents_before);
}

/// Without a live intent a rebuild states nothing of the engine.
#[test]
fn rebuild_with_no_intent_restates_nothing() {
    let w = World::on();
    w.put(
        10,
        vec![contact(1, 0xada, |_| {}), account_record(2, |_| {})],
    );
    w.events();
    let blobs = tempfile::tempdir().unwrap();
    crate::maintenance::rebuild_events(&w.f.db, &w.f.account, blobs.path(), 600, &w.wake).unwrap();
    let names: Vec<&'static str> = w.events().iter().map(BackendEvent::name).collect();
    assert_eq!(names, ["AccountReady", "AccountCaps"]);
}

// ---------------------------------------------------------------------------------------------
// `storage::sync` judges a write in flight before it merges (X4, on the real sync)
// ---------------------------------------------------------------------------------------------

/// A Storage Service that serves one manifest and its items; every other route fails.
struct StorageServer {
    /// The encoded `StorageManifest`.
    manifest: Vec<u8>,
    /// Encrypted values by raw identifier.
    items: std::collections::HashMap<Vec<u8>, Vec<u8>>,
    /// Holds the read of the items until the test lets it go (review B1's window).
    hold: Option<Hold>,
}

/// Where the scripted Storage Service waits inside `PUT /v1/storage/read`: it says it got there,
/// then waits to be released.
struct Hold {
    reached: std::sync::Arc<Notify>,
    release: std::sync::Arc<Notify>,
}

struct StorageSocket;

impl crate::net::ChatSocket for StorageSocket {
    async fn request(
        &self,
        _request: crate::net::ChatRequest,
    ) -> std::result::Result<crate::net::ChatResponse, crate::net::TransportError> {
        Ok(crate::net::ChatResponse {
            status: http::StatusCode::OK,
            message: None,
            headers: http::HeaderMap::new(),
            body: bytes::Bytes::from_static(br#"{"username":"u","password":"p"}"#),
        })
    }

    async fn send_multi_recipient(
        &self,
        _send: crate::net::MultiRecipientSend,
    ) -> std::result::Result<crate::net::MultiRecipientOutcome, crate::net::TransportError> {
        Err(crate::net::TransportError::Disconnected)
    }

    async fn get_upload_form(
        &self,
        _upload_length: u64,
    ) -> std::result::Result<
        std::result::Result<crate::net::UploadForm, crate::net::UploadTooLarge>,
        crate::net::TransportError,
    > {
        Err(crate::net::TransportError::Disconnected)
    }

    async fn disconnect(&self) {}
}

impl crate::net::SignalTransport for StorageServer {
    type Socket = StorageSocket;

    async fn connect_auth(
        &self,
        _credentials: &crate::net::AuthCredentials,
    ) -> std::result::Result<
        crate::net::Connected<StorageSocket, crate::net::ChatEvent>,
        crate::net::TransportError,
    > {
        Err(crate::net::TransportError::Disconnected)
    }

    async fn connect_unauth(
        &self,
    ) -> std::result::Result<
        crate::net::Connected<StorageSocket, crate::net::ChatEvent>,
        crate::net::TransportError,
    > {
        Err(crate::net::TransportError::Disconnected)
    }

    async fn connect_provisioning(
        &self,
    ) -> std::result::Result<
        crate::net::Connected<StorageSocket, crate::net::ProvisioningEvent>,
        crate::net::TransportError,
    > {
        Err(crate::net::TransportError::Disconnected)
    }

    async fn https(
        &self,
        request: crate::net::HttpsRequest,
    ) -> std::result::Result<crate::net::HttpsResponse, crate::net::TransportError> {
        let ok = |body: Vec<u8>| crate::net::HttpsResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: bytes::Bytes::from(body),
        };
        if request.method == http::Method::GET && request.path.starts_with("/v1/storage/manifest") {
            return Ok(ok(self.manifest.clone()));
        }
        if request.method == http::Method::PUT && request.path == "/v1/storage/read" {
            if let Some(hold) = &self.hold {
                hold.reached.notify_one();
                hold.release.notified().await;
            }
            let wanted =
                proto::ReadOperation::decode(&request.body.expect("a body")[..]).expect("a read");
            let items = wanted
                .read_key
                .iter()
                .filter_map(|k| {
                    self.items.get(k).map(|v| proto::StorageItem {
                        key: k.clone(),
                        value: v.clone(),
                    })
                })
                .collect();
            return Ok(ok(proto::StorageItems { items }.encode_to_vec()));
        }
        Err(crate::net::TransportError::Disconnected)
    }

    async fn https_download(
        &self,
        _request: crate::net::HttpsRequest,
        _sink: crate::net::DownloadSink,
    ) -> std::result::Result<http::StatusCode, crate::net::TransportError> {
        Err(crate::net::TransportError::Disconnected)
    }

    async fn https_upload(
        &self,
        _request: crate::net::UploadRequest,
    ) -> std::result::Result<crate::net::HttpsResponse, crate::net::TransportError> {
        Err(crate::net::TransportError::Disconnected)
    }

    fn network_changed(&self) {}
}

/// X4 on the real `storage::sync`: a write of ours was in flight when the app stopped, the server
/// applied it (version 8 lists our new record), and a newer Unarchive click was filed after the
/// plan. The sync fetches that manifest and merges our own record like any other, but only after
/// it judged the write by the same manifest and ran the landing zone: the click survives, nothing
/// is stated, the row is gone, and the sync's other work (the version, the new record) is done.
#[tokio::test]
async fn a_sync_lands_a_write_in_flight_before_it_merges() {
    use crate::storage::{item_key, manifest_key};
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = World::on();
            w.put(
                10,
                vec![contact(1, 0xada, |_| {}), account_record(2, |_| {})],
            );
            w.f.db
                .zone(|tx| -> Result<()> {
                    tx.execute("UPDATE account SET manifest_version = 7", [])?;
                    Ok(())
                })
                .unwrap();
            assert_eq!(w.archive(&chat(0xada), true, 100), Ok(()));
            let archive = w.intent(&person(0xada), ARCHIVED).unwrap();
            w.row(
                8,
                &[0xa1],
                vec![carried(&person(0xada), ARCHIVED, Some(1), archive.seq)],
                0,
            );
            w.gap_click(
                &person(0xada),
                ARCHIVED,
                Some(0),
                Some(0),
                Some(1),
                archive.seq + 1,
            );
            w.events();

            // The server: version 8 lists our record under its new id and the account record.
            let storage_key = [9u8; 32];
            let ikm = vec![9u8; 32];
            let ours = contact(0xa1, 0xada, |c| c.archived = true);
            let manifest = proto::ManifestRecord {
                version: 8,
                source_device: 2,
                identifiers: vec![
                    proto::manifest_record::Identifier {
                        raw: ours.key.clone(),
                        r#type: ItemType::Contact as i32,
                    },
                    proto::manifest_record::Identifier {
                        raw: key(2),
                        r#type: ItemType::Account as i32,
                    },
                ],
                record_ikm: ikm.clone(),
            };
            let sealed =
                |plain: &[u8], key: &[u8; 32]| crate::crypto::encrypt_gcm(key, [3; 12], plain);
            let server = StorageServer {
                manifest: proto::StorageManifest {
                    version: 8,
                    value: sealed(&manifest.encode_to_vec(), &manifest_key(&storage_key, 8)),
                }
                .encode_to_vec(),
                items: [(
                    ours.key.clone(),
                    sealed(&ours.raw, &item_key(&storage_key, Some(&ikm), &ours.key)),
                )]
                .into(),
                hold: None,
            };
            let test = crate::actor::testing::shared_over(
                std::sync::Arc::new(server),
                w.f.db.clone(),
                w.f.account.clone(),
                Vec::new(),
            );
            let shared = Rc::new(test.shared);
            let _core = test.core;
            let socket = std::sync::Arc::new(StorageSocket);

            let done = crate::storage::sync(&shared, &socket).await.unwrap();
            assert!(matches!(done, crate::jobs::Done::Finished));

            assert_eq!(
                w.in_flight_rows(),
                0,
                "the write was judged and its row retired"
            );
            let kept = w
                .intent(&person(0xada), ARCHIVED)
                .expect("the newer click survived the merge of our own record");
            assert_eq!((kept.value, kept.base), (Some(0), Some(1)));
            let version: Option<i64> =
                w.f.db
                    .read(|c| -> Result<_> {
                        Ok(c.query_row("SELECT manifest_version FROM account", [], |r| r.get(0))?)
                    })
                    .unwrap();
            assert_eq!(version, Some(8), "the sync went on to merge version 8");
            let stored: i64 =
                w.f.db
                    .read(|c| -> Result<_> {
                        Ok(c.query_row(
                            "SELECT count(*) FROM storage_records WHERE key = ?1",
                            [&ours.key],
                            |r| r.get(0),
                        )?)
                    })
                    .unwrap();
            assert_eq!(stored, 1, "our own record was merged like any other");
            let events = w.events();
            assert!(
                patch_of(&events, &chat(0xada)).is_none_or(|p| p.archived.is_none()),
                "nothing is stated for the held field: {events:?}"
            );
        })
        .await;
}

/// M5, the last link of the clock chain (the lead's BQ3 "one clock"): the sync stamps what it
/// merges with the clock the actor was handed in its context (`Shared::clock`), not with the
/// wall clock, so it compares with the core's `asked_at` (read from the same injected clock) as
/// N-C's rule says. A `FakeClock` far from today stands for the context's clock.
#[tokio::test]
async fn the_sync_stamps_what_it_merges_with_the_clock_of_the_actors_context() {
    use crate::storage::{item_key, manifest_key};
    use kourier_backend_api::{Clock as _, FakeClock};
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = World::new();
            w.put(
                10,
                vec![contact(1, 0xada, |_| {}), account_record(2, |_| {})],
            );
            w.f.db
                .zone(|tx| -> Result<()> {
                    tx.execute("UPDATE account SET manifest_version = 7", [])?;
                    Ok(())
                })
                .unwrap();
            let storage_key = [9u8; 32];
            let ikm = vec![9u8; 32];
            let fresh = contact(0xb1, 0xb0b, |_| {});
            let manifest = proto::ManifestRecord {
                version: 8,
                source_device: 2,
                identifiers: vec![
                    proto::manifest_record::Identifier {
                        raw: key(1),
                        r#type: ItemType::Contact as i32,
                    },
                    proto::manifest_record::Identifier {
                        raw: key(2),
                        r#type: ItemType::Account as i32,
                    },
                    proto::manifest_record::Identifier {
                        raw: fresh.key.clone(),
                        r#type: ItemType::Contact as i32,
                    },
                ],
                record_ikm: ikm.clone(),
            };
            let sealed =
                |plain: &[u8], key: &[u8; 32]| crate::crypto::encrypt_gcm(key, [3; 12], plain);
            let server = StorageServer {
                manifest: proto::StorageManifest {
                    version: 8,
                    value: sealed(&manifest.encode_to_vec(), &manifest_key(&storage_key, 8)),
                }
                .encode_to_vec(),
                items: [(
                    fresh.key.clone(),
                    sealed(&fresh.raw, &item_key(&storage_key, Some(&ikm), &fresh.key)),
                )]
                .into(),
                hold: None,
            };
            let clock = FakeClock::new(1_900_000_000_123);
            let test = crate::actor::testing::shared_over_clock(
                std::sync::Arc::new(server),
                w.f.db.clone(),
                w.f.account.clone(),
                Vec::new(),
                std::sync::Arc::new(clock.clone()),
            );
            let shared = Rc::new(test.shared);
            let _core = test.core;
            let socket = std::sync::Arc::new(StorageSocket);

            let done = crate::storage::sync(&shared, &socket).await.unwrap();
            assert!(matches!(done, crate::jobs::Done::Finished));

            let at = i64::try_from(clock.now_ms()).unwrap();
            let stamped: i64 =
                w.f.db
                    .read(|c| -> Result<_> {
                        Ok(c.query_row(
                            "SELECT updated_at FROM storage_records WHERE key = ?1",
                            [&fresh.key],
                            |r| r.get(0),
                        )?)
                    })
                    .expect("the new record was merged");
            assert_eq!(
                stamped, at,
                "the merge zone is stamped by the context's clock"
            );
            let synced: i64 =
                w.f.db
                    .read(|c| -> Result<_> {
                        Ok(c.query_row("SELECT storage_synced_at FROM account", [], |r| r.get(0))?)
                    })
                    .unwrap();
            assert_eq!(synced, at, "so is the time the sync finished");
        })
        .await;
}

// ---------------------------------------------------------------------------------------------
// The fix round of the review (B1, H1, H2, L1): the landing and the merge of what it landed are
// one moment, X1's ride-along is carried, `archived`'s base is the told base
// ---------------------------------------------------------------------------------------------

impl World {
    /// Sets `account.manifest_version` (the version the stored records are of).
    fn set_version(&self, version: i64) {
        self.f
            .db
            .zone(|tx| -> Result<()> {
                tx.execute("UPDATE account SET manifest_version = ?1", [version])?;
                Ok(())
            })
            .unwrap();
    }

    fn version(&self) -> Option<i64> {
        self.f
            .db
            .read(|c| -> Result<_> {
                Ok(c.query_row("SELECT manifest_version FROM account", [], |r| r.get(0))?)
            })
            .unwrap()
    }

    /// A fetched manifest at `version` that lists every stored record.
    fn fetched(&self, version: u64) -> proto::ManifestRecord {
        let identifiers = self
            .f
            .db
            .read(|c| -> Result<Vec<(Vec<u8>, i32)>> {
                let mut stmt = c.prepare("SELECT key, kind FROM storage_records ORDER BY rowid")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<kourier_db::rusqlite::Result<_>>()?;
                Ok(rows)
            })
            .unwrap()
            .into_iter()
            .map(|(raw, kind)| proto::manifest_record::Identifier { raw, r#type: kind })
            .collect();
        proto::ManifestRecord {
            version,
            source_device: 2,
            identifiers,
            record_ikm: vec![9; 32],
        }
    }

    /// What the write job does before its PUT (A10c): the real plan of every live intent from the
    /// stored records and the told bases, and the in-flight row of it. New ids `A1 A1 ..`, then
    /// on from the last one handed out.
    fn plan_and_row(&self, fetched: &proto::ManifestRecord, now: i64) -> (write::Plan, InFlight) {
        let mut n = self.last_id.get();
        let planned = self
            .f
            .db
            .zone(|tx| -> Result<_> {
                fill_targets(tx, &self.f.account)?;
                let intents = all_intents(tx)?;
                let targets = write::touched_targets(&intents);
                let stored = write::load_stored(tx, &targets)?;
                let told = told_archived(tx, &self.f.account, &targets, now)?;
                let plan = write::plan(
                    &write::Input {
                        fetched,
                        stored: &stored,
                        intents: &intents,
                        told_archived: &told,
                        pin_limit: 4,
                        device_id: 3,
                    },
                    &mut || {
                        n += 1;
                        [n; 16]
                    },
                )
                .expect("a plan");
                let row = InFlight::of(&plan, storage_generation(tx)?, now);
                write::write_in_flight(tx, &row)?;
                Ok((plan, row))
            })
            .unwrap();
        self.last_id.set(n);
        planned
    }

    /// The 200 zone of A10c: the written records stored, then the landing.
    fn land_written(&self, row: &InFlight, plan: &write::Plan, now: i64) -> Landing {
        self.f
            .db
            .zone(|tx| land_written(tx, &self.f.account, row, plan, now, &self.wake))
            .unwrap()
    }

    /// A1's hook of a message (or an own send) in the 1:1 chat with the person `n`.
    fn message_from(&self, n: u128, now: i64) {
        self.f
            .db
            .zone(|tx| {
                crate::receive::unarchive::after_own_send(
                    tx,
                    &self.f.account,
                    now,
                    &chat(n),
                    &self.wake,
                )
            })
            .unwrap();
    }
}

/// A row of `storage_records`: key, kind, target, record, updated_at.
type StoredRow = (Vec<u8>, i32, Option<String>, Vec<u8>, i64);

/// The record a plan inserts, as a sync would read it back.
fn decrypted_of(insert: &write::Insert) -> Decrypted {
    Decrypted {
        key: insert.key.clone(),
        kind: insert.kind as i32,
        record: proto::StorageRecord::decode(&insert.record[..]).unwrap(),
        raw: Zeroizing::new(insert.record.to_vec()),
    }
}

/// A scripted Storage Service at `version` that lists `listed` and serves `items`, sealed with the
/// fixture's storage key.
fn server_listing(
    version: u64,
    listed: &[(Vec<u8>, ItemType)],
    items: &[&Decrypted],
) -> StorageServer {
    use crate::storage::{item_key, manifest_key};
    let storage_key = [9u8; 32];
    let ikm = vec![9u8; 32];
    let manifest = proto::ManifestRecord {
        version,
        source_device: 2,
        identifiers: listed
            .iter()
            .map(|(raw, kind)| proto::manifest_record::Identifier {
                raw: raw.clone(),
                r#type: *kind as i32,
            })
            .collect(),
        record_ikm: ikm.clone(),
    };
    let sealed = |plain: &[u8], key: &[u8; 32]| crate::crypto::encrypt_gcm(key, [3; 12], plain);
    StorageServer {
        manifest: proto::StorageManifest {
            version,
            value: sealed(
                &manifest.encode_to_vec(),
                &manifest_key(&storage_key, version),
            ),
        }
        .encode_to_vec(),
        items: items
            .iter()
            .map(|d| {
                (
                    d.key.clone(),
                    sealed(&d.raw, &item_key(&storage_key, Some(&ikm), &d.key)),
                )
            })
            .collect(),
        hold: None,
    }
}

/// Runs the real `storage::sync` over `server`. With `during`, the server holds the read of the
/// items and `during` runs while it waits (a click, a reading or an unpin made then), on the same
/// thread as the actor's commands.
async fn sync_over(w: &World, mut server: StorageServer, during: Option<&dyn Fn(&World)>) {
    let reached = std::sync::Arc::new(Notify::new());
    let release = std::sync::Arc::new(Notify::new());
    if during.is_some() {
        server.hold = Some(Hold {
            reached: reached.clone(),
            release: release.clone(),
        });
    }
    let test = crate::actor::testing::shared_over(
        std::sync::Arc::new(server),
        w.f.db.clone(),
        w.f.account.clone(),
        Vec::new(),
    );
    let shared = Rc::new(test.shared);
    let _core = test.core;
    let socket = std::sync::Arc::new(StorageSocket);
    let sync =
        tokio::task::spawn_local(async move { crate::storage::sync(&shared, &socket).await });
    if let Some(during) = during {
        tokio::time::timeout(std::time::Duration::from_secs(60), reached.notified())
            .await
            .expect("the sync reads the items");
        during(w);
        release.notify_one();
    }
    let done = sync.await.unwrap().unwrap();
    assert!(matches!(done, crate::jobs::Done::Finished));
}

/// Review B1 (after a kill), a click: a write of Bob's mute reached the server (version 8 lists
/// our record) and the app was killed before its 200 zone. The sync after the restart reads the
/// items; WHILE it waits for them he unmutes Bob. The write was still in flight then, so the
/// Unmute's base is the written mute; the landing runs in the first merge zone, right before the
/// merge of our own record, and the Unmute survives it. (With the landing in a zone of its own
/// before the read, the Unmute takes the replaced record's "not muted" as the phone's value and
/// our own record wins.)
#[tokio::test]
async fn a_click_while_the_landing_sync_reads_the_items_stays() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = World::on();
            w.put(
                10,
                vec![contact(1, 0xb0b, |_| {}), account_record(2, |_| {})],
            );
            w.set_version(7);
            assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
            let mute = w.intent(&person(0xb0b), MUTED_UNTIL).unwrap();
            w.row(
                8,
                &[0xa1],
                vec![carried(&person(0xb0b), MUTED_UNTIL, Some(9_000), mute.seq)],
                0,
            );
            w.events();
            let ours = contact(0xa1, 0xb0b, |c| c.muted_until_timestamp = 9_000);
            let server = server_listing(
                8,
                &[
                    (ours.key.clone(), ItemType::Contact),
                    (key(2), ItemType::Account),
                ],
                &[&ours],
            );

            sync_over(
                &w,
                server,
                Some(&|w: &World| {
                    assert_eq!(w.mute(&chat(0xb0b), None, 150), Ok(()));
                    w.events();
                }),
            )
            .await;

            assert_eq!(w.in_flight_rows(), 0, "the write landed");
            let kept = w
                .intent(&person(0xb0b), MUTED_UNTIL)
                .expect("his Unmute survived the merge of our own record");
            assert_eq!((kept.value, kept.base), (None, Some(9_000)));
            let events = w.events();
            assert!(
                patch_of(&events, &chat(0xb0b)).is_none_or(|p| p.muted_until == Patch::Unchanged),
                "the written mute is not stated back"
            );
            assert_eq!(w.told(0xb0b).map(|r| r.muted_until), Some(None));
            assert_eq!(w.version(), Some(8));
        })
        .await;
}

/// Review B1 (after a kill), a reading: Mark as Unread of Chiara was written and landed; WHILE the
/// sync reads the items he opens her chat (the core's durable clear). The clear is filed against
/// the written mark and survives the merge of our own record: the chat he read is not marked
/// again. (With the landing before the read, the replaced record says "not marked", the clear is
/// dropped as having nothing to clear, and the merge states our own mark.)
#[tokio::test]
async fn a_reading_while_the_landing_sync_reads_the_items_stays() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = World::on();
            w.put(
                10,
                vec![contact(1, 0xc1a, |_| {}), account_record(2, |_| {})],
            );
            w.set_version(7);
            assert_eq!(w.mark(&chat(0xc1a), true, 100), Ok(()));
            let mark = w.intent(&person(0xc1a), MARKED_UNREAD).unwrap();
            w.row(
                8,
                &[0xa1],
                vec![carried(&person(0xc1a), MARKED_UNREAD, Some(1), mark.seq)],
                0,
            );
            w.events();
            let ours = contact(0xa1, 0xc1a, |c| c.marked_unread = true);
            let server = server_listing(
                8,
                &[
                    (ours.key.clone(), ItemType::Contact),
                    (key(2), ItemType::Account),
                ],
                &[&ours],
            );

            sync_over(
                &w,
                server,
                Some(&|w: &World| {
                    assert_eq!(w.automatic(&chat(0xc1a), 150, 150), Automatic::Filed);
                }),
            )
            .await;

            assert_eq!(w.in_flight_rows(), 0, "the write landed");
            let clear = w
                .intent(&person(0xc1a), MARKED_UNREAD)
                .expect("the clear survived the merge of our own record");
            assert_eq!(
                (clear.value, clear.base, clear.automatic),
                (Some(0), Some(1), true)
            );
            let events = w.events();
            assert!(
                patch_of(&events, &chat(0xc1a)).is_none_or(|p| p.marked_unread != Some(true)),
                "the chat he read is not marked again"
            );
        })
        .await;
}

/// Review B1 (after a kill), an unpin: Pin Ann was written and landed; WHILE the sync reads the
/// items he unpins her. The unpin is filed against the written pin list and survives the merge
/// of our own account record: no pin list after it names Ann. (With the landing before the read,
/// the replaced account record does not list Ann, the unpin's base says "not pinned", and our own
/// record pins her again.)
#[tokio::test]
async fn an_unpin_while_the_landing_sync_reads_the_items_stays() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = World::on();
            w.put(
                10,
                vec![contact(1, 0xa2a, |_| {}), account_record(2, |_| {})],
            );
            w.set_version(7);
            assert_eq!(w.pin(&chat(0xa2a), true, 100), Ok(()));
            let field = pin_field(&person(0xa2a));
            let pin = w.intent(ACCOUNT_TARGET, &field).unwrap();
            w.row(
                8,
                &[0xa1],
                vec![carried(ACCOUNT_TARGET, &field, Some(1), pin.seq)],
                0,
            );
            w.events();
            let ours = account_record(0xa1, |a| a.pinned_conversations = vec![contact_pin(0xa2a)]);
            let server = server_listing(
                8,
                &[
                    (key(1), ItemType::Contact),
                    (ours.key.clone(), ItemType::Account),
                ],
                &[&ours],
            );

            sync_over(
                &w,
                server,
                Some(&|w: &World| assert_eq!(w.pin(&chat(0xa2a), false, 150), Ok(()))),
            )
            .await;

            assert_eq!(w.in_flight_rows(), 0, "the write landed");
            let unpin = w
                .intent(ACCOUNT_TARGET, &field)
                .expect("the unpin survived the merge of our own account record");
            assert_eq!((unpin.value, unpin.base), (Some(0), Some(1)));
            let lists = pin_lists(&w.events());
            assert!(!lists.is_empty(), "the unpin stated a list");
            assert!(
                lists.iter().all(|list| !list.contains(&chat(0xa2a))),
                "Ann is never pinned again: {} lists",
                lists.len()
            );
        })
        .await;
}

/// Review B1 (the 200 path, A10c's call): `land_written` stores what the write put on the server
/// in the zone of the 200: the replaced record is gone, ours is stored under its new id with its
/// target and the written bytes, `manifest_version` is the written version, the carried intent is
/// closed and the row deleted. A click right after it reads the written record as the phone's
/// value, and a sync of that version merges nothing (the click stays, nothing is stated).
#[tokio::test]
async fn the_200_zone_stores_what_it_wrote() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = World::on();
            w.put(
                10,
                vec![contact(1, 0xb0b, |_| {}), account_record(2, |_| {})],
            );
            w.set_version(7);
            assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
            w.events();
            let (plan, row) = w.plan_and_row(&w.fetched(7), 110);
            assert_eq!(
                (plan.inserts.len(), plan.deletes.clone()),
                (1, vec![key(1)])
            );
            let ours = &plan.inserts[0];

            assert_eq!(
                w.land_written(&row, &plan, 120),
                Landing::Landed {
                    closed: 1,
                    moved: 0
                }
            );
            assert_eq!(w.in_flight_rows(), 0);
            assert!(w.intents().is_empty(), "the mute is closed");
            let rows: Vec<StoredRow> =
                w.f.db
                    .read(|c| -> Result<_> {
                        let mut stmt = c.prepare(
                            "SELECT key, kind, target, record, updated_at FROM storage_records
                             ORDER BY key",
                        )?;
                        let rows = stmt
                            .query_map([], |r| {
                                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                            })?
                            .collect::<kourier_db::rusqlite::Result<_>>()?;
                        Ok(rows)
                    })
                    .unwrap();
            assert!(
                rows.iter().all(|(k, ..)| *k != key(1)),
                "the replaced record is gone"
            );
            let stored = rows
                .iter()
                .find(|(k, ..)| *k == ours.key)
                .expect("our record is stored under its new id");
            assert_eq!(stored.1, ItemType::Contact as i32);
            assert_eq!(stored.2.as_deref(), Some(person(0xb0b).as_str()));
            assert_eq!(stored.3, ours.record.to_vec(), "the bytes written");
            assert_eq!(stored.4, 120);
            assert_eq!(rows.len(), 2, "ours and the untouched account record");
            assert_eq!(w.version(), Some(8), "the written version");

            // A click right after it: the phone's value is the written mute.
            assert_eq!(w.mute(&chat(0xb0b), None, 200), Ok(()));
            let click = w.intent(&person(0xb0b), MUTED_UNTIL).unwrap();
            assert_eq!((click.value, click.base), (None, Some(9_000)));
            w.events();

            // A sync of the written version finds nothing new.
            let server = server_listing(
                8,
                &[
                    (ours.key.clone(), ItemType::Contact),
                    (key(2), ItemType::Account),
                ],
                &[&decrypted_of(ours)],
            );
            sync_over(&w, server, None).await;
            assert_eq!(w.intent(&person(0xb0b), MUTED_UNTIL), Some(click));
            assert!(w.events().is_empty(), "nothing merged, nothing stated");
        })
        .await;
}

/// `land_written` after a `reset` during the PUT stores nothing (the generation moved) and
/// lands `Stale`; one whose version a sync already merged stores nothing twice.
#[test]
fn the_200_zone_stores_nothing_for_a_stale_or_merged_write() {
    let w = World::on();
    w.put(
        10,
        vec![contact(1, 0xb0b, |_| {}), account_record(2, |_| {})],
    );
    w.set_version(7);
    assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
    let (plan, row) = w.plan_and_row(&w.fetched(7), 110);
    w.f.db.zone(|tx| crate::storage::reset(tx, 500)).unwrap();
    assert_eq!(w.land_written(&row, &plan, 600), Landing::Stale);
    assert_eq!(w.f.db.read(|c| count(c, "storage_records")).unwrap(), 0);
    assert_eq!(w.version(), None);

    let w = World::on();
    w.put(
        10,
        vec![contact(1, 0xb0b, |_| {}), account_record(2, |_| {})],
    );
    w.set_version(7);
    assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
    let (plan, row) = w.plan_and_row(&w.fetched(7), 110);
    // A sync merged version 8 meanwhile (the landing found by it, our record merged). Review N4
    // (this half changed from `Landed { closed: 1 }`): records that cannot be stored are not
    // landed again: nothing is done, the mute is not closed a second time.
    w.set_version(8);
    let mute = w.intent(&person(0xb0b), MUTED_UNTIL);
    assert!(mute.is_some());
    assert_eq!(w.land_written(&row, &plan, 600), Landing::AlreadyJudged);
    assert_eq!(
        w.intent(&person(0xb0b), MUTED_UNTIL),
        mute,
        "nothing closed"
    );
    assert_eq!(
        w.f.db
            .read(|c| -> Result<i64> {
                Ok(c.query_row(
                    "SELECT count(*) FROM storage_records WHERE key = ?1",
                    [&plan.inserts[0].key],
                    |r| r.get(0),
                )?)
            })
            .unwrap(),
        0,
        "nothing stored over a merged version"
    );
    assert_eq!(w.version(), Some(8), "never moved back");
}

/// Review H1, sequence B (the critic's X1, second form): Bob is archived everywhere. A mute
/// rotates his record and X1 writes his told `archived = 1` along with it, carried by the write.
/// During the PUT a message from Bob brings him back here: as the write archives him on the phone
/// (Android applies the record in full), `archived = false` follows it, base 1. Our own record
/// merged after the landing leaves that alone and states nothing, and the next write unarchives
/// him on the phone.
#[test]
fn a_message_during_a_write_that_carries_a_told_archive_files_the_unarchive() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xb0b, |c| c.archived = true),
            account_record(2, |_| {}),
        ],
    );
    assert_eq!(w.told(0xb0b).map(|r| r.archived), Some(true));
    assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
    let (plan, _row) = w.plan_and_row(&w.fetched(7), 110);
    assert!(
        plan.planned
            .iter()
            .any(|p| p.rides && p.field == ARCHIVED && p.value == Some(1) && p.seq == 0),
        "the told archive rides along and is carried"
    );
    w.events();

    w.message_from(0xb0b, 150);
    let undo = w
        .intent(&person(0xb0b), ARCHIVED)
        .expect("archived = false follows the write");
    assert_eq!(
        (undo.value, undo.base, undo.automatic),
        (Some(0), Some(1), false)
    );
    assert!(w.jobs() >= 1);
    let events = w.events();
    assert_eq!(
        patch_of(&events, &chat(0xb0b)).unwrap().archived,
        Some(false),
        "the core hears A1's unarchive"
    );

    // The write lands (found after a restart), then our own record is merged.
    assert_eq!(
        w.resolve(&manifest_listing(8, &[0xa1]), 300),
        Resolved::Found {
            landing: Landing::Landed {
                closed: 1,
                moved: 1
            },
            assumed: false
        }
    );
    let events = w.merge(310, vec![decrypted_of(&plan.inserts[0])]);
    let after = w
        .intent(&person(0xb0b), ARCHIVED)
        .expect("the unarchive stays");
    assert_eq!((after.value, after.base), (Some(0), Some(1)));
    assert!(
        patch_of(&events, &chat(0xb0b)).is_none_or(|p| p.archived.is_none()),
        "our own record does not archive the chat with the new message"
    );
    assert_eq!(w.told(0xb0b).map(|r| r.archived), Some(false));

    // The next write unarchives him on the phone.
    let (next, _) = w.plan_and_row(&w.fetched(8), 400);
    assert_eq!(
        next.planned
            .iter()
            .map(|p| (p.field.as_str(), p.value, p.seq == after.seq, p.rides))
            .collect::<Vec<_>>(),
        [("archived", Some(0), true, false)]
    );
}

/// Review H1, sequence A: a message brought Bob back here (the told base says not archived, the
/// record still says archived); a mute rotates his record and X1 writes `archived = 0` along with
/// it. During the PUT he archives Bob: the Archive's base is the value the write sets, the
/// landing keeps it there, and our own record (archived 0) leaves his Archive alone.
#[test]
fn a_click_during_a_write_takes_the_ride_along_as_its_base() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xb0b, |c| c.archived = true),
            account_record(2, |_| {}),
        ],
    );
    w.message_from(0xb0b, 50);
    assert_eq!(w.told(0xb0b).map(|r| r.archived), Some(false));
    assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
    let (plan, _row) = w.plan_and_row(&w.fetched(7), 110);
    assert!(
        plan.planned
            .iter()
            .any(|p| p.rides && p.field == ARCHIVED && p.value == Some(0)),
        "the told base rides along"
    );
    w.events();

    assert_eq!(w.archive(&chat(0xb0b), true, 150), Ok(()));
    let archive = w.intent(&person(0xb0b), ARCHIVED).unwrap();
    assert_eq!((archive.value, archive.base), (Some(1), Some(0)));
    w.events();

    assert_eq!(
        w.resolve(&manifest_listing(8, &[0xa1]), 300),
        Resolved::Found {
            landing: Landing::Landed {
                closed: 1,
                moved: 1
            },
            assumed: false
        }
    );
    let events = w.merge(310, vec![decrypted_of(&plan.inserts[0])]);
    let kept = w
        .intent(&person(0xb0b), ARCHIVED)
        .expect("his Archive survives our own record");
    assert_eq!((kept.value, kept.base), (Some(1), Some(0)));
    assert!(
        patch_of(&events, &chat(0xb0b)).is_none_or(|p| p.archived.is_none()),
        "the core is not told 'unarchived'"
    );
}

/// Review H2: a message brought Bob back on both devices (Kourier's told base moved, Android
/// unarchived locally and wrote nothing; his record still says archived). He archives Bob here:
/// the Archive's base is the told base. The phone then rotates Bob's record for another reason
/// and writes its thread's state, not archived: that is OLDER than his click (`theirs == base`),
/// so his Archive stays and nothing is stated; a phone re-archive later is done elsewhere.
#[test]
fn a_phone_rotation_after_a1s_unarchive_does_not_undo_a_newer_archive() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xb0b, |c| c.archived = true),
            account_record(2, |_| {}),
        ],
    );
    w.message_from(0xb0b, 50);
    w.events();

    assert_eq!(w.archive(&chat(0xb0b), true, 200), Ok(()));
    let archive = w.intent(&person(0xb0b), ARCHIVED).unwrap();
    assert_eq!(
        (archive.value, archive.base, archive.shown),
        (Some(1), Some(0), Some(0)),
        "the phone's value is the told base, not the record's flag"
    );
    w.events();

    let events = w.merge(300, vec![contact(3, 0xb0b, |c| c.archived = false)]);
    let kept = w
        .intent(&person(0xb0b), ARCHIVED)
        .expect("his newer Archive stays");
    assert_eq!(kept.value, Some(1));
    assert!(
        patch_of(&events, &chat(0xb0b)).is_none_or(|p| p.archived.is_none()),
        "the core keeps showing his Archive"
    );
    assert_eq!(w.told(0xb0b).map(|r| r.archived), Some(true));

    w.merge(400, vec![contact(4, 0xb0b, |c| c.archived = true)]);
    assert!(
        w.intent(&person(0xb0b), ARCHIVED).is_none(),
        "the phone archived him too: done elsewhere"
    );
}

/// Review H2, the switch-off form: the same Archive over a told base that A1 moved; the switch goes
/// off. `drop_all` restates the phone's value, the told base (Bob in the inbox), not the stored
/// record's flag, and the mute dropped beside it restates its own base.
#[test]
fn turning_off_restates_the_told_base_of_an_archive() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xb0b, |c| c.archived = true),
            account_record(2, |_| {}),
        ],
    );
    w.message_from(0xb0b, 50);
    assert_eq!(w.archive(&chat(0xb0b), true, 200), Ok(()));
    assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 201), Ok(()));
    w.events();

    w.switch(false);
    let dropped =
        w.f.db
            .zone(|tx| drop_all(tx, &w.f.account, 300, &w.wake))
            .unwrap();
    assert_eq!(dropped, 2);
    let patch = patch_of(&w.events(), &chat(0xb0b)).expect("restated");
    assert_eq!(
        patch.archived,
        Some(false),
        "the phone shows Bob in its inbox"
    );
    assert_eq!(patch.muted_until, Patch::Clear);
    assert_eq!(
        w.told(0xb0b),
        Some(Roaming {
            archived: false,
            muted_until: None
        })
    );
}

/// Review L1: the switch went off while a write was on its way that archives Ada (her Archive)
/// and Bob (X1's told archive riding along with his mute). A message from each brings them back
/// here; the write lands. The landing states what was written (Bob's mute) but not the archive a
/// message overtook: both chats stay in the inbox here.
#[test]
fn a_landing_with_the_switch_off_leaves_a_chat_a_message_brought_back_in_the_inbox() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xada, |_| {}),
            contact(2, 0xb0b, |c| c.archived = true),
            account_record(3, |_| {}),
        ],
    );
    w.set_version(7);
    assert_eq!(w.archive(&chat(0xada), true, 100), Ok(()));
    assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 101), Ok(()));
    let (plan, row) = w.plan_and_row(&w.fetched(7), 110);
    w.switch(false);
    let dropped =
        w.f.db
            .zone(|tx| drop_all(tx, &w.f.account, 120, &w.wake))
            .unwrap();
    assert_eq!(dropped, 0, "both are carried");
    w.message_from(0xada, 150);
    w.message_from(0xb0b, 151);
    assert_eq!(w.intents().len(), 2, "nothing is filed with the switch off");
    w.events();

    assert_eq!(
        w.land_written(&row, &plan, 200),
        Landing::Landed {
            closed: 2,
            moved: 0
        }
    );
    let events = w.events();
    assert!(
        patch_of(&events, &chat(0xada)).is_none_or(|p| p.archived != Some(true)),
        "Ada stays in the inbox"
    );
    let bob = patch_of(&events, &chat(0xb0b)).expect("Bob's written mute is stated");
    assert_eq!(bob.muted_until, Patch::Set(9_000));
    assert_ne!(bob.archived, Some(true), "Bob stays in the inbox");
    assert_eq!(w.told(0xada).map(|r| r.archived), Some(false));
    assert_eq!(w.told(0xb0b).map(|r| r.archived), Some(false));
}

// ---------------------------------------------------------------------------------------------
// Round 2 of the fix round (the re-check's N1-N6): a write that did not land puts back what it
// lent, the identity base of a new key, `land_written` lands only its own write, our own records
// ---------------------------------------------------------------------------------------------

impl World {
    /// `storage_records.ours` of the record `key`.
    fn ours_of(&self, key: &[u8]) -> i64 {
        self.f
            .db
            .read(|c| -> Result<i64> {
                Ok(c.query_row(
                    "SELECT ours FROM storage_records WHERE key = ?1",
                    [key],
                    |r| r.get(0),
                )?)
            })
            .unwrap()
    }
}

/// Review N1 (a), the review's sequence through the real `storage::sync`: Mute Bob 1 h was
/// planned (version 8) and the app died before the PUT reached the server; the phone wrote
/// version 8 itself (a nickname, no mute). The sync after the restart judges the write NOT landed
/// right after the manifest decrypts, BEFORE it reads the items: a newer mute picked while they
/// are read finds no row and takes the phone's "not muted" as its base, so the merge of the
/// phone's record leaves it alone. (Judged in the first merge zone instead, the click would take
/// the planned 1 h as its base.)
#[tokio::test]
async fn a_write_that_did_not_land_is_judged_before_the_items_are_read() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = World::on();
            w.put(
                10,
                vec![contact(1, 0xb0b, |_| {}), account_record(2, |_| {})],
            );
            w.set_version(7);
            assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
            let _ = w.plan_and_row(&w.fetched(7), 110);
            w.events();
            let phones = contact(0xb5, 0xb0b, |c| c.given_name = "B".to_owned());
            let server = server_listing(
                8,
                &[
                    (phones.key.clone(), ItemType::Contact),
                    (key(2), ItemType::Account),
                ],
                &[&phones],
            );

            sync_over(
                &w,
                server,
                Some(&|w: &World| {
                    assert_eq!(w.in_flight_rows(), 0, "judged not landed before the read");
                    assert_eq!(w.mute(&chat(0xb0b), Some(50_000), 150), Ok(()));
                    let click = w.intent(&person(0xb0b), MUTED_UNTIL).unwrap();
                    assert_eq!(
                        (click.value, click.base),
                        (Some(50_000), None),
                        "the phone's value, not the planned 1 h"
                    );
                    w.events();
                }),
            )
            .await;

            let kept = w
                .intent(&person(0xb0b), MUTED_UNTIL)
                .expect("his newer mute survives the phone's record");
            assert_eq!((kept.value, kept.base), (Some(50_000), None));
            let events = w.events();
            assert!(
                patch_of(&events, &chat(0xb0b)).is_none_or(|p| p.muted_until == Patch::Unchanged),
                "the core is not told 'unmuted'"
            );
            assert_eq!(w.told(0xb0b).map(|r| r.muted_until), Some(Some(50_000)));
            assert_eq!(w.version(), Some(8));
        })
        .await;
}

/// Review N1 (b): a click during the PUT takes the value the write sets as its base (X3). The
/// write does NOT land (a sync's verdict, or A10c's non-2xx through `write_not_landed`): the
/// base goes back to the phone's value before the write (`Planned::prior`), so the phone's record
/// that still holds that value leaves his click alone.
#[test]
fn a_click_during_a_write_that_did_not_land_gets_the_phones_value_back_as_its_base() {
    let staged = || {
        let w = World::on();
        w.put(
            10,
            vec![contact(1, 0xb0b, |_| {}), account_record(2, |_| {})],
        );
        w.set_version(7);
        assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
        let (plan, row) = w.plan_and_row(&w.fetched(7), 110);
        assert!(
            plan.planned
                .iter()
                .any(|p| p.field == MUTED_UNTIL && p.value == Some(9_000) && p.prior.is_none()),
            "the write keeps the phone's value before it"
        );
        assert_eq!(w.mute(&chat(0xb0b), Some(50_000), 150), Ok(()));
        let click = w.intent(&person(0xb0b), MUTED_UNTIL).unwrap();
        assert_eq!(click.base, Some(9_000), "X3: the value the write sets");
        w.events();
        (w, row)
    };

    // A sync finds the phone wrote version 8 first.
    let (w, _row) = staged();
    assert_eq!(
        w.resolve(&manifest_listing(8, &[0x01]), 300),
        Resolved::NotLanded
    );
    let click = w.intent(&person(0xb0b), MUTED_UNTIL).unwrap();
    assert_eq!(
        (click.value, click.base),
        (Some(50_000), None),
        "the base the write lent is given back"
    );
    let events = w.merge(
        310,
        vec![contact(5, 0xb0b, |c| c.given_name = "B".to_owned())],
    );
    assert_eq!(
        w.intent(&person(0xb0b), MUTED_UNTIL).map(|i| i.value),
        Some(Some(50_000)),
        "his mute stays over the phone's unchanged record"
    );
    assert!(
        patch_of(&events, &chat(0xb0b)).is_none_or(|p| p.muted_until == Patch::Unchanged),
        "nothing is stated"
    );

    // A10c's non-2xx: the same through the one shared function; a second answer finds it judged.
    let (w, row) = staged();
    assert!(
        w.f.db
            .zone(|tx| write_not_landed(tx, &w.f.account, &row, 300, &w.wake))
            .unwrap()
    );
    assert_eq!(w.in_flight_rows(), 0);
    assert_eq!(
        w.intent(&person(0xb0b), MUTED_UNTIL).map(|i| i.base),
        Some(None)
    );
    assert!(
        !w.f.db
            .zone(|tx| write_not_landed(tx, &w.f.account, &row, 301, &w.wake))
            .unwrap()
    );
}

/// Review N1 (b) with the switch off (M2's variant and N2): Mute Bob 1 h is carried, X1's told
/// `archived = 0` rides along; during the PUT he picks Mute 8 h and archives Bob, then turns the
/// switch off: `drop_all` deletes both clicks unstated (the landing was to state the written
/// values). The write does not land: nothing carries anything, so the phone's values before the
/// write are restated, and Kourier shows what the phone shows.
#[test]
fn a_write_that_did_not_land_with_the_switch_off_restates_the_phones_values() {
    let w = World::on();
    w.put(
        10,
        vec![contact(1, 0xb0b, |_| {}), account_record(2, |_| {})],
    );
    w.set_version(7);
    assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
    let (plan, _row) = w.plan_and_row(&w.fetched(7), 110);
    assert!(
        plan.planned
            .iter()
            .any(|p| p.rides && p.field == ARCHIVED && p.prior == Some(0)),
        "Bob's told archive rides along with its own value as the prior"
    );
    assert_eq!(w.mute(&chat(0xb0b), Some(50_000), 150), Ok(()));
    assert_eq!(w.archive(&chat(0xb0b), true, 151), Ok(()));
    w.events();
    w.switch(false);
    let dropped =
        w.f.db
            .zone(|tx| drop_all(tx, &w.f.account, 200, &w.wake))
            .unwrap();
    assert_eq!(dropped, 2, "both clicks are on carried fields");
    assert!(
        w.events().is_empty(),
        "the landing was to state the written values"
    );

    assert_eq!(
        w.resolve(&manifest_listing(7, &[0xa1]), 300),
        Resolved::NotLanded
    );
    assert!(w.intents().is_empty());
    let bob = patch_of(&w.events(), &chat(0xb0b))
        .expect("the phone's values from before the write are restated");
    assert_eq!(bob.muted_until, Patch::Clear);
    assert_eq!(bob.archived, Some(false));
    assert_eq!(
        w.told(0xb0b),
        Some(Roaming {
            archived: false,
            muted_until: None
        })
    );
}

/// Review N3: Bob's record holds K1, DEFAULT; his key changed to K2 (DEFAULT here: a new key
/// after DEFAULT stays DEFAULT, on the phone too) and he verifies K2 here. The base is K2's state
/// here before the click, so the phone's first record of K2 with its automatic DEFAULT is older
/// than his Verify: it stays and is not reverted.
#[test]
fn a_verify_of_a_new_key_stays_over_the_phones_automatic_state_of_it() {
    let new_key = || {
        libsignal_protocol::IdentityKeyPair::generate(&mut rand::rng())
            .identity_key()
            .serialize()
            .to_vec()
    };
    let (k1, k2) = (new_key(), new_key());
    let sid = ServiceId::from(aci(0xb0b)).service_id_string();
    let w = World::on();
    w.f.db
        .zone(|tx| -> Result<()> {
            tx.execute(
                "INSERT INTO identities(service_id, identity_key, verified, first_use, updated_at)
                 VALUES (?1, ?2, 0, 0, 1)",
                (&sid, &k2[..]),
            )?;
            peers::set_features_hash(tx, &sid, 7, 1)?;
            Ok(())
        })
        .unwrap();
    w.put(
        10,
        vec![contact(1, 0xb0b, |c| {
            c.identity_key = k1.clone();
            c.identity_state = proto::contact_record::IdentityState::Default as i32;
        })],
    );
    w.f.db
        .zone(|tx| -> Result<bool> {
            tx.execute(
                "UPDATE identities SET verified = 1 WHERE service_id = ?1",
                [&sid],
            )?;
            intend_identity(tx, &w.f.account, &sid, 1, 0, &k2, 100).map(IdentityFiled::is_filed)
        })
        .unwrap()
        .then_some(())
        .expect("filed");
    let intent = w.intent(&person(0xb0b), IDENTITY).unwrap();
    assert_eq!(
        (intent.value, intent.base),
        (Some(1), Some(0)),
        "K2's state here before the click"
    );
    w.events();

    let events = w.merge(
        200,
        vec![contact(2, 0xb0b, |c| {
            c.identity_key = k2.clone();
            c.identity_state = proto::contact_record::IdentityState::Default as i32;
        })],
    );
    assert!(
        w.intent(&person(0xb0b), IDENTITY).is_some(),
        "his Verify stays"
    );
    assert_eq!(verified_of(&w, 0xb0b), 1, "and is not reverted");
    assert_eq!(verified_notices(&events), 0);
}

/// Review N4: a sync judged our write first (the phone wrote version 9 after it: counted as
/// landed) and merged the phone's Bob muted 2 h; he unmutes Bob over it. The 200 of our older write
/// arrives late: `land_written` finds the stored row gone and does nothing: the unmute keeps the
/// phone's 2 h as its base, our records are not stored, and with the switch off our older 1 h is
/// not stated over the phone's 2 h.
#[test]
fn the_200_zone_does_not_land_a_write_a_sync_judged_first() {
    let staged = || {
        let w = World::on();
        w.put(
            10,
            vec![contact(1, 0xb0b, |_| {}), account_record(2, |_| {})],
        );
        w.set_version(7);
        assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
        let (plan, row) = w.plan_and_row(&w.fetched(7), 110);
        assert_eq!(
            w.resolve(&manifest_listing(9, &[0x02]), 200),
            Resolved::Found {
                landing: Landing::Landed {
                    closed: 1,
                    moved: 0
                },
                assumed: true
            }
        );
        w.merge(
            210,
            vec![contact(3, 0xb0b, |c| c.muted_until_timestamp = 20_000)],
        );
        w.set_version(9);
        w.events();
        (w, plan, row)
    };

    let (w, plan, row) = staged();
    assert_eq!(w.mute(&chat(0xb0b), None, 220), Ok(()));
    let click = w.intent(&person(0xb0b), MUTED_UNTIL).unwrap();
    assert_eq!(click.base, Some(20_000));
    w.events();
    assert_eq!(w.land_written(&row, &plan, 300), Landing::AlreadyJudged);
    assert_eq!(
        w.intent(&person(0xb0b), MUTED_UNTIL),
        Some(click),
        "its base is not moved to our older written mute"
    );
    assert!(w.events().is_empty());
    assert_eq!(
        w.f.db
            .read(|c| -> Result<i64> {
                Ok(c.query_row(
                    "SELECT count(*) FROM storage_records WHERE key = ?1",
                    [&plan.inserts[0].key],
                    |r| r.get(0),
                )?)
            })
            .unwrap(),
        0,
        "our records are not stored"
    );
    assert_eq!(w.version(), Some(9));

    let (w, plan, row) = staged();
    w.switch(false);
    assert_eq!(w.land_written(&row, &plan, 300), Landing::AlreadyJudged);
    assert!(
        patch_of(&w.events(), &chat(0xb0b)).is_none(),
        "the phone's 2 h stands"
    );
    assert_eq!(w.told(0xb0b).map(|r| r.muted_until), Some(Some(20_000)));
}

/// Review N6: Mark as Unread of Chiara was written; he opened her chat while the backend was down,
/// and the core resends the durable clear with its older `asked_at` after our write landed. Our
/// own record (marked) is newer than the reading, but it is OURS (`storage_records.ours`, set by
/// the 200 zone and, after a kill, by the merge of a key the landed write inserted): N-C does not
/// keep it, and the clear is filed. A phone record (a new key) is the phone's again: its newer
/// mark stays.
#[tokio::test]
async fn the_reading_does_not_take_our_own_record_for_the_phones_mark() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let staged = || {
                let w = World::on();
                w.put(
                    10,
                    vec![contact(1, 0xc1a, |_| {}), account_record(2, |_| {})],
                );
                w.set_version(7);
                assert_eq!(w.mark(&chat(0xc1a), true, 100), Ok(()));
                let (plan, row) = w.plan_and_row(&w.fetched(7), 110);
                w.events();
                (w, plan, row)
            };

            // The 200 path.
            let (w, plan, row) = staged();
            assert_eq!(
                w.land_written(&row, &plan, 200),
                Landing::Landed {
                    closed: 1,
                    moved: 0
                }
            );
            assert_eq!(w.ours_of(&plan.inserts[0].key), 1);
            assert_eq!(w.automatic(&chat(0xc1a), 150, 210), Automatic::Filed);
            let clear = w.intent(&person(0xc1a), MARKED_UNREAD).unwrap();
            assert_eq!((clear.value, clear.automatic), (Some(0), true));
            // The phone marks her again (a new record of its own): that newer mark stays.
            w.merge(300, vec![contact(5, 0xc1a, |c| c.marked_unread = true)]);
            assert_eq!(w.ours_of(&key(5)), 0);
            assert_eq!(w.automatic(&chat(0xc1a), 250, 310), Automatic::PhoneKept);

            // After a kill: the sync lands the write and merges our own record.
            let (w, plan, _row) = staged();
            let ours = decrypted_of(&plan.inserts[0]);
            let server = server_listing(
                8,
                &[
                    (ours.key.clone(), ItemType::Contact),
                    (key(2), ItemType::Account),
                ],
                &[&ours],
            );
            sync_over(&w, server, None).await;
            assert_eq!(w.in_flight_rows(), 0, "landed");
            assert_eq!(w.ours_of(&ours.key), 1, "the merge names our own record");
            assert_eq!(w.ours_of(&key(2)), 0);
            w.events();
            assert_eq!(w.automatic(&chat(0xc1a), 150, 400), Automatic::Filed);
        })
        .await;
}

/// Review L1, the after-kill half: the switch went off while a write that archives Ada was on its
/// way; a message from Ada brought her back here; the app was killed before the 200. The sync
/// after the restart lands the write (its statement leaves out the overtaken archive) and merges
/// our own record, which says archived: that merge states no flag, so Ada stays in the inbox.
#[tokio::test]
async fn a_landing_after_a_kill_leaves_a_chat_a_message_brought_back_in_the_inbox() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = World::on();
            w.put(
                10,
                vec![contact(1, 0xada, |_| {}), account_record(2, |_| {})],
            );
            w.set_version(7);
            assert_eq!(w.archive(&chat(0xada), true, 100), Ok(()));
            let (plan, _row) = w.plan_and_row(&w.fetched(7), 110);
            w.switch(false);
            let dropped =
                w.f.db
                    .zone(|tx| drop_all(tx, &w.f.account, 120, &w.wake))
                    .unwrap();
            assert_eq!(dropped, 0, "carried");
            w.message_from(0xada, 150);
            assert_eq!(w.told(0xada).map(|r| r.archived), Some(false));
            w.events();
            let ours = decrypted_of(&plan.inserts[0]);
            let server = server_listing(
                8,
                &[
                    (ours.key.clone(), ItemType::Contact),
                    (key(2), ItemType::Account),
                ],
                &[&ours],
            );

            sync_over(&w, server, None).await;

            assert_eq!(w.in_flight_rows(), 0, "landed");
            assert!(w.intents().is_empty());
            assert_eq!(w.ours_of(&ours.key), 1);
            let events = w.events();
            assert!(
                patch_of(&events, &chat(0xada)).is_none_or(|p| p.archived != Some(true)),
                "our own record does not archive the chat a message brought back"
            );
            assert_eq!(w.told(0xada).map(|r| r.archived), Some(false));
        })
        .await;
}

// ---------------------------------------------------------------------------------------------
// Round 3 (the lead's calls on the second re-check): Stale is not "not landed", R2-1 (`shown`
// goes back with the base), R2-2 (1) and (3), R2-3 (a behind manifest proves nothing)
// ---------------------------------------------------------------------------------------------

/// Round 3, "Stale is not not landed": Mute Bob 1 h was written, he picked Mute 8 h during the
/// PUT (its base is the written 1 h, X3), a storage key change (`reset`) came before the 200. The
/// write DID land: the full sync after the reset merges the phone's records, which hold the
/// written 1 h, and his 8 h stays (`theirs == base`). A Stale landing puts nothing back to
/// `prior` (judged "not landed", the base would go back to "not muted" and the phone would win
/// over the click).
#[test]
fn a_stale_landing_leaves_a_newer_clicks_base_the_written_value() {
    let w = World::on();
    w.put(
        10,
        vec![contact(1, 0xb0b, |_| {}), account_record(2, |_| {})],
    );
    w.set_version(7);
    assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
    let (plan, row) = w.plan_and_row(&w.fetched(7), 110);
    assert_eq!(w.mute(&chat(0xb0b), Some(50_000), 150), Ok(()));
    let click = w.intent(&person(0xb0b), MUTED_UNTIL).unwrap();
    assert_eq!(
        (click.value, click.base, click.shown),
        (Some(50_000), Some(9_000), Some(9_000)),
        "X3: the value the write sets"
    );
    w.events();

    w.f.db.zone(|tx| crate::storage::reset(tx, 500)).unwrap();
    assert_eq!(w.land_written(&row, &plan, 600), Landing::Stale);
    assert_eq!(
        w.intent(&person(0xb0b), MUTED_UNTIL),
        Some(click),
        "base and shown stay the written value"
    );
    assert_eq!(w.in_flight_rows(), 0);

    // The full sync: the phone's records hold the 1 h the write set.
    let events = w.merge(
        700,
        vec![contact(5, 0xb0b, |c| c.muted_until_timestamp = 9_000)],
    );
    let kept = w
        .intent(&person(0xb0b), MUTED_UNTIL)
        .expect("his 8 h stays over the written 1 h");
    assert_eq!((kept.value, kept.base), (Some(50_000), Some(9_000)));
    assert!(
        patch_of(&events, &chat(0xb0b)).is_none_or(|p| p.muted_until == Patch::Unchanged),
        "nothing is stated"
    );
    assert_eq!(w.told(0xb0b).map(|r| r.muted_until), Some(Some(50_000)));
}

/// Review R2-1: Mute Bob 1 h is carried, he picks Mute 8 h during the PUT (base and `shown` are
/// the 1 h the write sets), the write does not land. The click gets the phone's value back as its
/// base AND as its `shown`: a click back to the 1 h is then a change to write, not an undo (L6) of
/// something that was never written (the phone would stay unmuted while Kourier showed 1 h). A
/// click back to the phone's own value is the undo.
#[test]
fn a_click_back_to_the_failed_writes_value_is_a_change_to_write() {
    let staged = || {
        let w = World::on();
        w.put(
            10,
            vec![contact(1, 0xb0b, |_| {}), account_record(2, |_| {})],
        );
        w.set_version(7);
        assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
        let (_plan, row) = w.plan_and_row(&w.fetched(7), 110);
        assert_eq!(w.mute(&chat(0xb0b), Some(50_000), 150), Ok(()));
        let click = w.intent(&person(0xb0b), MUTED_UNTIL).unwrap();
        assert_eq!((click.base, click.shown), (Some(9_000), Some(9_000)));
        assert!(
            w.f.db
                .zone(|tx| write_not_landed(tx, &w.f.account, &row, 200, &w.wake))
                .unwrap()
        );
        let given = w.intent(&person(0xb0b), MUTED_UNTIL).unwrap();
        assert_eq!(
            (given.value, given.base, given.shown),
            (Some(50_000), None, None),
            "the phone's value, twice"
        );
        w.events();
        w
    };

    // Back to the 1 h that was never written.
    let w = staged();
    assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 300), Ok(()));
    let again = w
        .intent(&person(0xb0b), MUTED_UNTIL)
        .expect("a change to write, not an undo");
    assert_eq!((again.value, again.base), (Some(9_000), None));
    assert_eq!(w.told(0xb0b).map(|r| r.muted_until), Some(Some(9_000)));

    // Back to what the phone has: nothing to write.
    let w = staged();
    assert_eq!(w.mute(&chat(0xb0b), None, 300), Ok(()));
    assert!(w.intent(&person(0xb0b), MUTED_UNTIL).is_none(), "undone");
    assert_eq!(w.told(0xb0b).map(|r| r.muted_until), Some(None));
}

/// Review R2-2 (1): Bob is archived on both. Unarchive Bob is carried by the write (`prior` 1);
/// during the PUT he archives Bob again, then a message from Bob brings him back here (A1 tells
/// the core, and deletes that Archive: the write in flight sets 0, so it archives nothing). The
/// switch goes off and the write does not land. A message marks EVERY `archived` entry the write
/// carries (the Unarchive's 0 too): nothing restates its `prior` 1, Bob stays in the inbox.
#[test]
fn a_message_overtakes_a_carried_unarchive_and_a_failed_write_does_not_archive_him_again() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xb0b, |c| c.archived = true),
            account_record(2, |_| {}),
        ],
    );
    assert_eq!(w.told(0xb0b).map(|r| r.archived), Some(true));
    assert_eq!(w.archive(&chat(0xb0b), false, 100), Ok(()));
    let (plan, _row) = w.plan_and_row(&w.fetched(7), 110);
    assert!(
        plan.planned
            .iter()
            .any(|p| p.field == ARCHIVED && !p.rides && p.value == Some(0) && p.prior == Some(1)),
        "the Unarchive is carried and the phone's value before it is 1"
    );
    assert_eq!(w.archive(&chat(0xb0b), true, 150), Ok(()));
    w.events();

    w.message_from(0xb0b, 160);
    assert!(
        w.intent(&person(0xb0b), ARCHIVED).is_none(),
        "the write in flight sets 0: the Archive is overtaken and deleted"
    );
    assert_eq!(w.told(0xb0b).map(|r| r.archived), Some(false));
    let stored = w.f.db.read(write::read_in_flight).unwrap().unwrap();
    assert!(
        stored
            .planned
            .iter()
            .any(|p| p.field == ARCHIVED && p.overtaken),
        "the message marked the carried Unarchive, whatever it sets"
    );
    w.switch(false);
    w.events();

    assert_eq!(
        w.resolve(&manifest_listing(7, &[0xa1]), 300),
        Resolved::NotLanded
    );
    assert!(w.intents().is_empty());
    assert!(
        patch_of(&w.events(), &chat(0xb0b)).is_none_or(|p| p.archived != Some(true)),
        "the phone's old 1 is not restated over the message"
    );
    assert_eq!(w.told(0xb0b).map(|r| r.archived), Some(false));
}

/// Review R2-2 (3): Bob is archived on both; Mute Bob 1 h is carried and X1's told `archived = 1`
/// rides along (`prior` 1). The switch goes off; then the phone unarchives Bob (a record of its
/// own: the merge stores it, states `archived = false` and the told base follows). The write does
/// not land: the ride-along's `prior` 1 is stated only while the newest stored record still says
/// it, so the phone's newer unarchive stands and Bob is not archived again. (The control is
/// N2's `a_write_that_did_not_land_with_the_switch_off_restates_the_phones_values`: there a click
/// moved the told base and the record still says the `prior`, which IS restated.)
#[test]
fn a_failed_write_restates_a_ride_along_only_while_the_newest_record_says_its_prior() {
    let w = World::on();
    w.put(
        10,
        vec![
            contact(1, 0xb0b, |c| c.archived = true),
            account_record(2, |_| {}),
        ],
    );
    assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
    let (plan, _row) = w.plan_and_row(&w.fetched(7), 110);
    assert!(
        plan.planned
            .iter()
            .any(|p| p.rides && p.field == ARCHIVED && p.prior == Some(1)),
        "the told archive rides along with its own value as the prior"
    );
    w.switch(false);
    let dropped =
        w.f.db
            .zone(|tx| drop_all(tx, &w.f.account, 120, &w.wake))
            .unwrap();
    assert_eq!(dropped, 0, "carried");
    w.events();

    let events = w.merge(
        200,
        vec![contact(5, 0xb0b, |c| {
            c.archived = false;
            c.given_name = "B".to_owned();
        })],
    );
    assert_eq!(
        patch_of(&events, &chat(0xb0b)).and_then(|p| p.archived),
        Some(false),
        "the phone's unarchive is stated"
    );
    assert_eq!(w.told(0xb0b).map(|r| r.archived), Some(false));

    assert_eq!(
        w.resolve(&manifest_listing(7, &[0xa1]), 300),
        Resolved::NotLanded
    );
    let events = w.events();
    assert!(
        patch_of(&events, &chat(0xb0b)).is_none_or(|p| p.archived != Some(true)),
        "the ride-along's 1 is not restated over the phone's newer 0"
    );
    assert_eq!(w.told(0xb0b).map(|r| r.archived), Some(false));
    // The mute the write carried was dropped and restated, as before.
    assert_eq!(
        patch_of(&events, &chat(0xb0b)).map(|p| p.muted_until),
        Some(Patch::Clear)
    );
}

/// Review R2-3: a sync whose GET was answered before the server applied our PUT fetches the
/// stored version 7, BEHIND the write (version 8). That proves nothing about the write: its row
/// stays (the PUT is on the wire; A10c's job judges it), the newer click keeps the write's value
/// as its base, and the 200 finds its row and lands as usual.
#[tokio::test]
async fn a_manifest_behind_the_write_does_not_judge_it() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = World::on();
            w.put(
                10,
                vec![contact(1, 0xb0b, |_| {}), account_record(2, |_| {})],
            );
            w.set_version(7);
            assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
            let (plan, row) = w.plan_and_row(&w.fetched(7), 110);
            assert_eq!(w.mute(&chat(0xb0b), Some(50_000), 150), Ok(()));
            w.events();

            let server = server_listing(
                7,
                &[(key(1), ItemType::Contact), (key(2), ItemType::Account)],
                &[],
            );
            sync_over(&w, server, None).await;

            assert_eq!(w.in_flight_rows(), 1, "a behind manifest judges nothing");
            let click = w.intent(&person(0xb0b), MUTED_UNTIL).unwrap();
            assert_eq!(
                (click.value, click.base),
                (Some(50_000), Some(9_000)),
                "the write's value stays his base"
            );

            // The 200 arrives and finds its row: the newer click's base moves to the written 1 h.
            assert_eq!(
                w.land_written(&row, &plan, 300),
                Landing::Landed {
                    closed: 0,
                    moved: 1
                }
            );
            assert_eq!(w.version(), Some(8));
            assert_eq!(w.ours_of(&plan.inserts[0].key), 1);
            assert_eq!(
                w.intent(&person(0xb0b), MUTED_UNTIL).map(|i| i.value),
                Some(Some(50_000))
            );
        })
        .await;
}

/// Review N4's `own` guard, decided alone (round 3): a sync judged the write NOT landed because
/// the phone wrote its very version (the fetched manifest of version 8 lists none of our
/// identifiers: the proof that stays after R2-3), the row is gone and the merge has not run yet
/// (`manifest_version` is still 7). A late 200 would pass `store_written` (7 + 1 == 8) and store
/// our records under the phone's version; the stored row is not this write any more, so nothing is
/// stored, closed or moved. (The real server answers such a PUT with a 409, so this is the guard
/// for a job that overlaps a sync, which A10c forbids.) A manifest BEHIND the write judges
/// nothing (the test before).
#[test]
fn the_200_zone_does_not_land_a_write_a_sync_judged_not_landed() {
    let w = World::on();
    w.put(
        10,
        vec![contact(1, 0xb0b, |_| {}), account_record(2, |_| {})],
    );
    w.set_version(7);
    assert_eq!(w.mute(&chat(0xb0b), Some(9_000), 100), Ok(()));
    let (plan, row) = w.plan_and_row(&w.fetched(7), 110);
    let mute = w.intent(&person(0xb0b), MUTED_UNTIL).unwrap();
    w.events();

    assert!(
        w.f.db
            .zone(|tx| resolve_not_landed(
                tx,
                &w.f.account,
                &manifest_listing(8, &[0x02]),
                200,
                &w.wake
            ))
            .unwrap(),
        "the phone wrote the write's own version: not landed"
    );
    assert_eq!(w.in_flight_rows(), 0);
    assert_eq!(w.version(), Some(7));

    assert_eq!(w.land_written(&row, &plan, 300), Landing::AlreadyJudged);
    assert_eq!(w.version(), Some(7), "the phone's version is not claimed");
    assert_eq!(
        w.f.db
            .read(|c| -> Result<i64> {
                Ok(c.query_row(
                    "SELECT count(*) FROM storage_records WHERE key = ?1",
                    [&plan.inserts[0].key],
                    |r| r.get(0),
                )?)
            })
            .unwrap(),
        0,
        "our records are not stored"
    );
    assert_eq!(
        w.intent(&person(0xb0b), MUTED_UNTIL),
        Some(mute),
        "the mute is kept, to be planned again"
    );
}
