//! The pure plan of a Storage Service write (A10b chunk B5). The inputs are built with prost's
//! own encoding functions and by hand, and the expected bytes are concatenations of the same
//! parts, so no expectation comes from the editor under test. Synthetic ids only.

use std::collections::HashMap;
use std::rc::Rc;

use libsignal_core::{Aci, ServiceId};
use prost::encoding::{WireType as ProstWire, encode_key, encode_varint};
use tokio::sync::Notify;
use uuid::Uuid;

use super::*;
use crate::proto::account_record::pinned_conversation::{self, Identifier as PinId};
use crate::receive::tests::fixture;
use crate::storage::{Decrypted, group_target, merge_zone};

// ---------------------------------------------------------------------------------------------
// Bytes
// ---------------------------------------------------------------------------------------------

fn tag(number: u32, wire_type: ProstWire) -> Vec<u8> {
    let mut out = Vec::new();
    encode_key(number, wire_type, &mut out);
    out
}

fn vint(number: u32, value: u64) -> Vec<u8> {
    let mut out = tag(number, ProstWire::Varint);
    encode_varint(value, &mut out);
    out
}

fn lenf(number: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = tag(number, ProstWire::LengthDelimited);
    encode_varint(payload.len() as u64, &mut out);
    out.extend_from_slice(payload);
    out
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn aci(n: u128) -> Aci {
    Aci::from(Uuid::from_u128(n))
}

fn sid(n: u128) -> String {
    ServiceId::from(aci(n)).service_id_string()
}

fn person(n: u128) -> String {
    format!("contact:{}", sid(n))
}

fn group_t(m: u8) -> String {
    group_target(&[m; 32])
}

fn id(n: u8) -> Vec<u8> {
    vec![n; 16]
}

/// A `ContactRecord` in a `StorageRecord`: `head` (fields below 22 and 25), a nickname holding
/// an unknown field of its own, the ACI, an unknown varint 999 and an unknown 998, and an unknown
/// field 50 of the `StorageRecord` itself.
fn contact_rec(n: u128, head: &[u8]) -> Vec<u8> {
    let nickname = lenf(22, &cat(&[&lenf(1, b"Given"), &vint(99, 1)]));
    let inner = cat(&[
        head,
        &nickname,
        &lenf(25, Uuid::from_u128(n).as_bytes()),
        &vint(999, 7),
        &lenf(998, b"opaque"),
    ]);
    cat(&[&lenf(1, &inner), &vint(50, 1)])
}

/// A `GroupV2Record`: the master key, `head` (fields 2 to 11), an unknown 999.
fn group_rec(m: u8, head: &[u8]) -> Vec<u8> {
    let inner = cat(&[&lenf(1, &[m; 32]), head, &vint(999, 3)]);
    cat(&[&lenf(3, &inner), &vint(50, 1)])
}

/// An `AccountRecord`: `head` (fields below 14), the pin entries (payloads of field 14), an
/// unknown 999.
fn account_rec(head: &[u8], entries: &[Vec<u8>]) -> Vec<u8> {
    let pins: Vec<u8> = entries.iter().flat_map(|e| lenf(14, e)).collect();
    let inner = cat(&[head, &pins, &vint(999, 3)]);
    cat(&[&lenf(4, &inner), &vint(50, 1)])
}

/// A pin entry in the old form (the string service id) with an unknown field of its own.
fn old_contact_entry(n: u128) -> Vec<u8> {
    let entry = PinnedConversation {
        identifier: Some(PinId::Contact(pinned_conversation::Contact {
            service_id: sid(n),
            ..Default::default()
        })),
    };
    cat(&[&entry.encode_to_vec(), &vint(77, 1)])
}

fn group_entry(m: u8) -> Vec<u8> {
    PinnedConversation {
        identifier: Some(PinId::GroupMasterKey(vec![m; 32])),
    }
    .encode_to_vec()
}

fn notes_entry() -> Vec<u8> {
    PinnedConversation {
        identifier: Some(PinId::ReleaseNotes(pinned_conversation::ReleaseNotes {})),
    }
    .encode_to_vec()
}

/// An entry that names nothing this build knows.
fn opaque_entry() -> Vec<u8> {
    vint(77, 1)
}

/// The entry the plan appends for a person (BQ5): the 16-byte ACI only.
fn new_contact_entry(n: u128) -> Vec<u8> {
    lenf(1, &lenf(3, Uuid::from_u128(n).as_bytes()))
}

/// The entry the plan appends for a group.
fn new_group_entry(m: u8) -> Vec<u8> {
    lenf(4, &[m; 32])
}

// ---------------------------------------------------------------------------------------------
// The case
// ---------------------------------------------------------------------------------------------

const CONTACT: i32 = ItemType::Contact as i32;
const GROUP: i32 = ItemType::Groupv2 as i32;
const ACCOUNT: i32 = ItemType::Account as i32;
const STORY: i32 = ItemType::StoryDistributionList as i32;
const STICKER: i32 = ItemType::StickerPack as i32;
/// A type this build does not know.
const FUTURE: i32 = 99;

fn manifest(version: u64, ids: &[(u8, i32)], record_ikm: &[u8]) -> proto::ManifestRecord {
    proto::ManifestRecord {
        version,
        source_device: 1,
        identifiers: ids
            .iter()
            .map(|&(n, t)| Identifier {
                raw: id(n),
                r#type: t,
            })
            .collect(),
        record_ikm: record_ikm.to_vec(),
    }
}

fn intent(target: &str, field: &str, value: Option<i64>, seq: i64) -> Intent {
    Intent {
        target: target.to_owned(),
        field: field.to_owned(),
        value,
        base: Some(0),
        shown: Some(0),
        key: None,
        seq,
        automatic: false,
        created_at: 0,
    }
}

fn pin(target: &str, chat: &str, on: bool, seq: i64) -> Intent {
    intent(target, &intents::pin_field(chat), Some(i64::from(on)), seq)
}

struct Case {
    fetched: proto::ManifestRecord,
    stored: Vec<StoredRecord>,
    intents: Vec<Intent>,
    told: HashMap<String, bool>,
    pin_limit: u32,
}

impl Case {
    /// A manifest at version 10 listing `ids`.
    fn new(ids: &[(u8, i32)], record_ikm: &[u8]) -> Self {
        Self {
            fetched: manifest(10, ids, record_ikm),
            stored: Vec::new(),
            intents: Vec::new(),
            told: HashMap::new(),
            pin_limit: 4,
        }
    }

    fn store(mut self, key: u8, target: &str, raw: Vec<u8>, updated_at: i64, rowid: i64) -> Self {
        self.stored.push(StoredRecord {
            key: id(key),
            target: target.to_owned(),
            peer: target.strip_prefix("contact:").map(str::to_owned),
            raw: Zeroizing::new(raw),
            updated_at,
            rowid,
        });
        self
    }

    fn intents(mut self, intents: Vec<Intent>) -> Self {
        self.intents = intents;
        self
    }

    fn told(mut self, target: &str, archived: bool) -> Self {
        self.told.insert(target.to_owned(), archived);
        self
    }

    fn limit(mut self, limit: u32) -> Self {
        self.pin_limit = limit;
        self
    }

    /// New identifiers: `A1 A1 ..`, `A2 A2 ..`, and so on.
    fn run_with(&self, new_id: &mut impl FnMut() -> [u8; 16]) -> Result<Plan, Refusal> {
        plan(
            &Input {
                fetched: &self.fetched,
                stored: &self.stored,
                intents: &self.intents,
                told_archived: &self.told,
                pin_limit: self.pin_limit,
                device_id: 7,
            },
            new_id,
        )
    }

    fn run(&self) -> Result<Plan, Refusal> {
        let mut n = 0xA0u8;
        self.run_with(&mut || {
            n += 1;
            [n; 16]
        })
    }

    fn plan(&self) -> Plan {
        self.run().unwrap()
    }
}

/// The new manifest's identifiers as (raw, type).
fn listed(plan: &Plan) -> Vec<(Vec<u8>, i32)> {
    plan.manifest
        .identifiers
        .iter()
        .map(|i| (i.raw.clone(), i.r#type))
        .collect()
}

fn planned_view(plan: &Plan) -> Vec<(String, i64, Option<i64>)> {
    plan.planned
        .iter()
        .map(|p| (p.field.clone(), p.seq, p.value))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Contact, group, account, two at once
// ---------------------------------------------------------------------------------------------

#[test]
fn a_contact_is_rotated_and_every_other_byte_is_kept() {
    let target = person(1);
    let before = contact_rec(1, &[]);
    let case = Case::new(&[(1, CONTACT), (2, STORY), (3, ACCOUNT)], &[9; 32])
        .store(1, &target, before.clone(), 100, 1)
        .intents(vec![
            intent(&target, intents::ARCHIVED, Some(1), 3),
            intent(&target, intents::MUTED_UNTIL, Some(5000), 4),
        ]);
    let plan = case.plan();

    // ONE new record under a new 16-byte identifier, the old one deleted.
    assert_eq!(plan.inserts.len(), 1);
    let insert = &plan.inserts[0];
    assert_eq!(insert.key, id(0xA1));
    assert_eq!(insert.kind, ItemType::Contact);
    assert_eq!(insert.target, target);
    assert_eq!(insert.peer.as_deref(), Some(sid(1).as_str()));
    // The same record with the two fields added: every other byte (the nickname's unknown
    // field, the unknown 999 and 998, the StorageRecord's own unknown 50) as it was.
    let expected = contact_rec(1, &cat(&[&vint(11, 1), &vint(13, 5000)]));
    assert_eq!(&insert.record[..], &expected[..]);
    assert_ne!(expected, before);
    let decoded = proto::StorageRecord::decode(&insert.record[..]).unwrap();
    let Some(Record::Contact(contact)) = decoded.record else {
        panic!("not a contact");
    };
    assert!(contact.archived);
    assert_eq!(contact.muted_until_timestamp, 5000);
    assert_eq!(contact.aci_binary, Uuid::from_u128(1).as_bytes().to_vec());
    assert_eq!(contact.nickname.unwrap().given, "Given");
    assert_eq!(plan.deletes, vec![id(1)]);

    // The manifest: fetched minus the deleted plus the inserted; version + 1; our device; the
    // fetched recordIkm.
    assert_eq!(
        listed(&plan),
        vec![(id(2), STORY), (id(3), ACCOUNT), (id(0xA1), CONTACT)]
    );
    assert_eq!(plan.manifest.version, 11);
    assert_eq!(plan.manifest.source_device, 7);
    assert_eq!(plan.manifest.record_ikm, vec![9; 32]);
    // What the in-flight row stores: the written values with the seqs they close.
    assert_eq!(
        planned_view(&plan),
        vec![
            ("archived".to_owned(), 3, Some(1)),
            ("muted_until".to_owned(), 4, Some(5000))
        ]
    );
    assert!(plan.left.is_empty());
    assert!(!plan.is_empty());
    // The stored bytes were not touched.
    assert_eq!(&case.stored[0].raw[..], &before[..]);
    validate(&case.fetched, &plan).unwrap();
}

#[test]
fn a_group_is_rotated_and_an_unmute_removes_the_field() {
    let target = group_t(5);
    // Blocked (2) and a mute until 7000 (6) are the phone's.
    let before = group_rec(5, &cat(&[&vint(2, 1), &vint(6, 7000)]));
    let case = Case::new(&[(1, GROUP), (2, CONTACT)], &[1; 32])
        .store(1, &target, before, 100, 1)
        .intents(vec![
            intent(&target, intents::MARKED_UNREAD, Some(1), 1),
            intent(&target, intents::MUTED_UNTIL, None, 2),
        ]);
    let plan = case.plan();
    assert_eq!(plan.inserts.len(), 1);
    assert_eq!(plan.inserts[0].kind, ItemType::Groupv2);
    assert_eq!(plan.inserts[0].target, target);
    // Blocked stays, the mark is inserted in its place, the mute is gone (0 = absent).
    let expected = group_rec(5, &cat(&[&vint(2, 1), &vint(5, 1)]));
    assert_eq!(&plan.inserts[0].record[..], &expected[..]);
    assert_eq!(plan.deletes, vec![id(1)]);
    assert_eq!(listed(&plan), vec![(id(2), CONTACT), (id(0xA1), GROUP)]);
    assert_eq!(
        planned_view(&plan),
        vec![
            ("marked_unread".to_owned(), 1, Some(1)),
            ("muted_until".to_owned(), 2, None)
        ]
    );
    validate(&case.fetched, &plan).unwrap();
}

#[test]
fn the_account_record_takes_the_note_to_self_flags_and_the_pins() {
    // Note to Self is archived on the phone; three pins (a person in the old form, a group,
    // the release notes).
    let before = account_rec(
        &vint(5, 1),
        &[old_contact_entry(2), group_entry(3), notes_entry()],
    );
    let case = Case::new(&[(1, ACCOUNT)], &[])
        .store(1, ACCOUNT_TARGET, before, 100, 1)
        .intents(vec![
            intent(ACCOUNT_TARGET, intents::ARCHIVED, Some(0), 1),
            intent(ACCOUNT_TARGET, intents::MARKED_UNREAD, Some(1), 2),
            pin(ACCOUNT_TARGET, &person(9), true, 5),
            pin(ACCOUNT_TARGET, &group_t(3), false, 6),
        ]);
    let plan = case.plan();
    assert_eq!(plan.inserts.len(), 1);
    // Unarchived (field 5 gone), marked (10), the group's pin dropped, the person's entry and the
    // release notes byte for byte (the unknown field of the entry included), the new person
    // last, in the phone's own form.
    let expected = account_rec(
        &vint(10, 1),
        &[old_contact_entry(2), notes_entry(), new_contact_entry(9)],
    );
    assert_eq!(&plan.inserts[0].record[..], &expected[..]);
    assert_eq!(plan.inserts[0].kind, ItemType::Account);
    assert_eq!(plan.deletes, vec![id(1)]);
    assert_eq!(listed(&plan), vec![(id(0xA1), ACCOUNT)]);
    assert_eq!(
        planned_view(&plan),
        vec![
            ("archived".to_owned(), 1, Some(0)),
            ("marked_unread".to_owned(), 2, Some(1)),
            (intents::pin_field(&person(9)), 5, Some(1)),
            (intents::pin_field(&group_t(3)), 6, Some(0)),
        ]
    );
    // A legacy manifest: absent stays absent.
    assert!(plan.manifest.record_ikm.is_empty());
    validate(&case.fetched, &plan).unwrap();
}

#[test]
fn two_targets_are_planned_at_once() {
    let (c, g) = (person(1), group_t(5));
    // A third contact is listed and stored but no intent names it.
    let case = Case::new(
        &[(1, CONTACT), (2, GROUP), (3, CONTACT), (4, ACCOUNT)],
        &[4; 32],
    )
    .store(1, &c, contact_rec(1, &[]), 100, 1)
    .store(2, &g, group_rec(5, &[]), 100, 2)
    .store(3, &person(3), contact_rec(3, &[]), 100, 3)
    .intents(vec![
        // The group's intent is the older one, but contacts come first in the manifest.
        intent(&g, intents::ARCHIVED, Some(1), 1),
        intent(&c, intents::MARKED_UNREAD, Some(1), 2),
    ]);
    let plan = case.plan();
    assert_eq!(plan.inserts.len(), 2);
    assert_eq!(plan.inserts[0].target, c);
    assert_eq!(plan.inserts[1].target, g);
    assert_eq!(plan.inserts[0].key, id(0xA1));
    assert_eq!(plan.inserts[1].key, id(0xA2));
    assert_eq!(
        &plan.inserts[0].record[..],
        &contact_rec(1, &vint(12, 1))[..]
    );
    assert_eq!(&plan.inserts[1].record[..], &group_rec(5, &vint(4, 1))[..]);
    assert_eq!(plan.deletes, vec![id(1), id(2)]);
    // The untouched contact and the account record stay as listed.
    assert_eq!(
        listed(&plan),
        vec![
            (id(3), CONTACT),
            (id(4), ACCOUNT),
            (id(0xA1), CONTACT),
            (id(0xA2), GROUP)
        ]
    );
    // One list for the write, oldest intent first.
    assert_eq!(
        planned_view(&plan),
        vec![
            ("archived".to_owned(), 1, Some(1)),
            ("marked_unread".to_owned(), 2, Some(1))
        ]
    );
    validate(&case.fetched, &plan).unwrap();
}

#[test]
fn failed_and_unknown_ids_stay_listed() {
    let target = person(1);
    // 2 has a type this build does not know, 3 is a contact record that failed to decrypt (not
    // stored), 4 is a sticker pack, 5 the account record (not stored either).
    let case = Case::new(
        &[
            (1, CONTACT),
            (2, FUTURE),
            (3, CONTACT),
            (4, STICKER),
            (5, ACCOUNT),
        ],
        &[2; 32],
    )
    .store(1, &target, contact_rec(1, &[]), 100, 1)
    .intents(vec![intent(&target, intents::ARCHIVED, Some(1), 1)]);
    let plan = case.plan();
    assert_eq!(
        listed(&plan),
        vec![
            (id(2), FUTURE),
            (id(3), CONTACT),
            (id(4), STICKER),
            (id(5), ACCOUNT),
            (id(0xA1), CONTACT)
        ]
    );
    assert_eq!(plan.deletes, vec![id(1)]);
    validate(&case.fetched, &plan).unwrap();
}

#[test]
fn nothing_to_write_is_an_empty_plan() {
    let case = Case::new(&[(1, CONTACT)], &[3; 32]).store(1, &person(1), contact_rec(1, &[]), 1, 1);
    let plan = case.plan();
    assert!(plan.is_empty());
    assert!(plan.deletes.is_empty());
    assert!(plan.planned.is_empty());
    assert_eq!(listed(&plan), vec![(id(1), CONTACT)]);
    validate(&case.fetched, &plan).unwrap();
}

// ---------------------------------------------------------------------------------------------
// X1: a rotation writes the told archived base
// ---------------------------------------------------------------------------------------------

#[test]
fn a_rotation_writes_the_told_archived_base() {
    // A message brought the chat back here (A1): the core shows it in the inbox, the stored
    // record still says archived. A mute rotates the record: it must not archive the chat again.
    let c = person(1);
    let case = Case::new(&[(1, CONTACT)], &[1; 32])
        .store(1, &c, contact_rec(1, &vint(11, 1)), 100, 1)
        .intents(vec![intent(&c, intents::MARKED_UNREAD, Some(1), 1)])
        .told(&c, false);
    let plan = case.plan();
    assert_eq!(
        &plan.inserts[0].record[..],
        &contact_rec(1, &vint(12, 1))[..],
        "field 11 comes from the told base (not archived), not from the record"
    );
    let Some(Record::Contact(contact)) = proto::StorageRecord::decode(&plan.inserts[0].record[..])
        .unwrap()
        .record
    else {
        panic!("not a contact");
    };
    assert!(!contact.archived);
    // The told base is carried beside the intent (review H1: it changed from "only the intent is
    // carried"): no intent asked for it (seq 0, `rides`), but the write sets it, so a click during
    // the PUT, the landing and a message during the PUT must see it.
    assert_eq!(
        planned_view(&plan),
        vec![
            ("archived".to_owned(), 0, Some(0)),
            ("marked_unread".to_owned(), 1, Some(1))
        ]
    );
    assert_eq!(
        plan.planned.iter().map(|p| p.rides).collect::<Vec<_>>(),
        [true, false]
    );
    assert_eq!(plan.planned[0].target, c);

    // The same for a group (field 4) ...
    let g = group_t(5);
    let case = Case::new(&[(1, GROUP)], &[])
        .store(1, &g, group_rec(5, &vint(4, 1)), 100, 1)
        .intents(vec![intent(&g, intents::MARKED_UNREAD, Some(1), 1)])
        .told(&g, false);
    assert_eq!(
        &case.plan().inserts[0].record[..],
        &group_rec(5, &vint(5, 1))[..]
    );
    // ... and for Note to Self (the account record's field 5), rotated for a pin.
    let case = Case::new(&[(1, ACCOUNT)], &[])
        .store(1, ACCOUNT_TARGET, account_rec(&vint(5, 1), &[]), 100, 1)
        .intents(vec![pin(ACCOUNT_TARGET, &person(9), true, 1)])
        .told(ACCOUNT_TARGET, false);
    assert_eq!(
        &case.plan().inserts[0].record[..],
        &account_rec(&[], &[new_contact_entry(9)])[..]
    );

    // A base that says archived is written too (the chat is in the archive here).
    let case = Case::new(&[(1, CONTACT)], &[])
        .store(1, &c, contact_rec(1, &[]), 100, 1)
        .intents(vec![intent(&c, intents::MUTED_UNTIL, Some(1000), 1)])
        .told(&c, true);
    assert_eq!(
        &case.plan().inserts[0].record[..],
        &contact_rec(1, &cat(&[&vint(11, 1), &vint(13, 1000)]))[..]
    );

    // No told base (a target the loader found none for): the record's flag stays.
    let case = Case::new(&[(1, CONTACT)], &[])
        .store(1, &c, contact_rec(1, &vint(11, 1)), 100, 1)
        .intents(vec![intent(&c, intents::MUTED_UNTIL, Some(1000), 1)]);
    assert_eq!(
        &case.plan().inserts[0].record[..],
        &contact_rec(1, &cat(&[&vint(11, 1), &vint(13, 1000)]))[..]
    );
}

#[test]
fn an_archive_intent_covers_the_told_base() {
    let c = person(1);
    // Archive clicked, a message brought the chat back meanwhile (the told base moved to false,
    // and `leaves_the_archive` would have deleted the intent: this is the plan's own rule for an
    // intent that is still there): the intent's value is written.
    let case = Case::new(&[(1, CONTACT)], &[])
        .store(1, &c, contact_rec(1, &[]), 100, 1)
        .intents(vec![intent(&c, intents::ARCHIVED, Some(1), 1)])
        .told(&c, false);
    assert_eq!(
        &case.plan().inserts[0].record[..],
        &contact_rec(1, &vint(11, 1))[..]
    );
    // And an unarchive over a base that says archived.
    let case = Case::new(&[(1, CONTACT)], &[])
        .store(1, &c, contact_rec(1, &vint(11, 1)), 100, 1)
        .intents(vec![intent(&c, intents::ARCHIVED, Some(0), 1)])
        .told(&c, true);
    let plan = case.plan();
    assert_eq!(&plan.inserts[0].record[..], &contact_rec(1, &[])[..]);
    assert_eq!(
        planned_view(&plan),
        vec![("archived".to_owned(), 1, Some(0))]
    );
}

// ---------------------------------------------------------------------------------------------
// M7: duplicates
// ---------------------------------------------------------------------------------------------

#[test]
fn duplicates_rotate_the_newest_and_delete_the_rest() {
    let c = person(1);
    // Four rows of one person. Each says another mute, so the one rotated can be told. Newest
    // = the latest merge time, then the later row: key 3 (300, rowid 5) beats key 2 (300, rowid
    // 2) and key 1 (100, rowid 9). Key 4 is stored but the manifest does not list it any more.
    let case = Case::new(
        &[(1, CONTACT), (2, CONTACT), (3, CONTACT), (6, CONTACT)],
        &[1; 32],
    )
    .store(1, &c, contact_rec(1, &vint(13, 111)), 100, 9)
    .store(2, &c, contact_rec(1, &vint(13, 222)), 300, 2)
    .store(3, &c, contact_rec(1, &vint(13, 333)), 300, 5)
    .store(4, &c, contact_rec(1, &vint(13, 444)), 900, 10)
    .store(6, &person(6), contact_rec(6, &[]), 100, 11)
    .intents(vec![intent(&c, intents::MARKED_UNREAD, Some(1), 1)]);
    let plan = case.plan();
    assert_eq!(
        plan.inserts.len(),
        1,
        "ONE record is written for the target"
    );
    assert_eq!(
        &plan.inserts[0].record[..],
        &contact_rec(1, &cat(&[&vint(12, 1), &vint(13, 333)]))[..],
        "the newest listed record is the one edited"
    );
    // All three listed ones go; the unlisted row is neither rotated nor deleted; the other
    // person is not touched.
    let mut deleted = plan.deletes.clone();
    deleted.sort();
    assert_eq!(deleted, vec![id(1), id(2), id(3)]);
    assert_eq!(listed(&plan), vec![(id(6), CONTACT), (id(0xA1), CONTACT)]);
    validate(&case.fetched, &plan).unwrap();
}

#[test]
fn two_account_records_are_kept_when_the_write_does_not_touch_them() {
    // The phone lists two account records; a contact is clicked. The write does not touch the
    // account record, so both stay listed as the phone had them (as the untouched duplicates of
    // any target do, M7), and the plan stands: no refusal (the lead's call on the review of
    // A10b; it used to be `TwoAccountRecords`).
    let case = Case::new(&[(1, CONTACT), (2, ACCOUNT), (3, ACCOUNT)], &[])
        .store(1, &person(1), contact_rec(1, &[]), 100, 1)
        .intents(vec![intent(&person(1), intents::ARCHIVED, Some(1), 1)]);
    let plan = case.plan();
    assert_eq!(
        listed(&plan),
        vec![(id(2), ACCOUNT), (id(3), ACCOUNT), (id(0xA1), CONTACT)],
        "both account records stay listed"
    );
    assert_eq!(
        plan.deletes,
        vec![id(1)],
        "only the contact's old record goes"
    );
    validate(&case.fetched, &plan).unwrap();
    // When the account record is rotated too (a pin), the newest is edited and the other
    // deleted: one is left, and the plan stands.
    let case = Case::new(&[(2, ACCOUNT), (3, ACCOUNT)], &[])
        .store(2, ACCOUNT_TARGET, account_rec(&vint(5, 1), &[]), 100, 1)
        .store(3, ACCOUNT_TARGET, account_rec(&[], &[]), 200, 2)
        .intents(vec![pin(ACCOUNT_TARGET, &person(9), true, 1)]);
    let plan = case.plan();
    assert_eq!(listed(&plan), vec![(id(0xA1), ACCOUNT)]);
    assert_eq!(
        &plan.inserts[0].record[..],
        &account_rec(&[], &[new_contact_entry(9)])[..]
    );
}

// ---------------------------------------------------------------------------------------------
// L2: a legacy manifest
// ---------------------------------------------------------------------------------------------

#[test]
fn a_legacy_manifest_stays_without_record_ikm() {
    let c = person(1);
    let case = Case::new(&[(1, CONTACT), (2, STORY)], &[])
        .store(1, &c, contact_rec(1, &[]), 100, 1)
        .intents(vec![intent(&c, intents::ARCHIVED, Some(1), 1)]);
    let plan = case.plan();
    // Absent stays absent: nothing is generated, the field is not on the wire.
    assert!(plan.manifest.record_ikm.is_empty());
    let encoded = plan.manifest.encode_to_vec();
    assert!(
        !wire::fields(&encoded)
            .unwrap()
            .iter()
            .any(|f| f.number == 4),
        "no field 4 (recordIkm) in the encoded manifest"
    );
    assert_eq!(plan.manifest.version, 11);
    validate(&case.fetched, &plan).unwrap();
    // A present recordIkm is kept to the byte.
    let ikm: Vec<u8> = (0..32).collect();
    let mut case = case;
    case.fetched.record_ikm = ikm.clone();
    let plan = case.plan();
    assert_eq!(plan.manifest.record_ikm, ikm);
    let encoded = plan.manifest.encode_to_vec();
    assert!(
        wire::fields(&encoded)
            .unwrap()
            .iter()
            .any(|f| f.number == 4)
    );
}

// ---------------------------------------------------------------------------------------------
// Validation (H6)
// ---------------------------------------------------------------------------------------------

/// A sound plan: one contact rotated, two other records listed.
fn sound() -> (Case, Plan) {
    let c = person(1);
    let case = Case::new(&[(1, CONTACT), (2, STORY), (3, ACCOUNT)], &[9; 32])
        .store(1, &c, contact_rec(1, &[]), 100, 1)
        .intents(vec![intent(&c, intents::ARCHIVED, Some(1), 1)]);
    let plan = case.plan();
    (case, plan)
}

#[test]
fn validate_accepts_the_plan_it_made() {
    let (case, plan) = sound();
    assert_eq!(validate(&case.fetched, &plan), Ok(()));
}

#[test]
fn validate_refuses_a_type_mismatch() {
    // The manifest calls the new contact record an account record.
    let (case, mut plan) = sound();
    plan.manifest
        .identifiers
        .iter_mut()
        .find(|i| i.raw == id(0xA1))
        .unwrap()
        .r#type = ACCOUNT;
    assert_eq!(validate(&case.fetched, &plan), Err(Refusal::TypeMismatch));
    // The insert says group, the record is a contact (the manifest follows the insert).
    let (case, mut plan) = sound();
    plan.inserts[0].kind = ItemType::Groupv2;
    plan.manifest
        .identifiers
        .iter_mut()
        .find(|i| i.raw == id(0xA1))
        .unwrap()
        .r#type = GROUP;
    assert_eq!(validate(&case.fetched, &plan), Err(Refusal::TypeMismatch));
    // The bytes of an insert are no record of the engine's kinds.
    let (case, mut plan) = sound();
    plan.inserts[0].record = Zeroizing::new(vint(1000, 1));
    assert_eq!(validate(&case.fetched, &plan), Err(Refusal::TypeMismatch));
    // In the plan itself: the manifest lists a stored contact record as a group, or the bytes
    // stored under a contact target are an account record: nothing is rotated.
    let c = person(1);
    let case = Case::new(&[(1, GROUP)], &[])
        .store(1, &c, contact_rec(1, &[]), 100, 1)
        .intents(vec![intent(&c, intents::ARCHIVED, Some(1), 1)]);
    assert_eq!(case.run().unwrap_err(), Refusal::TypeMismatch);
    let case = Case::new(&[(1, CONTACT)], &[])
        .store(1, &c, account_rec(&[], &[]), 100, 1)
        .intents(vec![intent(&c, intents::ARCHIVED, Some(1), 1)]);
    assert_eq!(case.run().unwrap_err(), Refusal::TypeMismatch);
}

#[test]
fn validate_refuses_a_duplicate_id() {
    let (case, mut plan) = sound();
    let copy = plan.manifest.identifiers[0].clone();
    plan.manifest.identifiers.push(copy);
    assert_eq!(validate(&case.fetched, &plan), Err(Refusal::DuplicateId));
    // The same in the plan: a new identifier that is one of the listed ones.
    let (case, _) = sound();
    let refused = case.run_with(&mut || [2; 16]);
    assert_eq!(refused.unwrap_err(), Refusal::DuplicateId);
}

#[test]
fn validate_refuses_two_account_records() {
    let (case, mut plan) = sound();
    plan.manifest.identifiers.push(Identifier {
        raw: id(0x77),
        r#type: ACCOUNT,
    });
    assert_eq!(
        validate(&case.fetched, &plan),
        Err(Refusal::TwoAccountRecords)
    );
}

#[test]
fn validate_refuses_two_account_records_once_the_account_record_is_touched() {
    // The phone lists two; the write rotates the account record (a pin). The plan deletes both
    // and lists the new one. A plan that rotated it but kept the old two (our bug) lists three:
    // refused, although the fetched manifest had two.
    let case = Case::new(&[(2, ACCOUNT), (3, ACCOUNT)], &[])
        .store(2, ACCOUNT_TARGET, account_rec(&vint(5, 1), &[]), 100, 1)
        .store(3, ACCOUNT_TARGET, account_rec(&[], &[]), 200, 2)
        .intents(vec![pin(ACCOUNT_TARGET, &person(9), true, 1)]);
    let mut plan = case.plan();
    validate(&case.fetched, &plan).unwrap();
    plan.deletes.retain(|d| *d != id(2));
    plan.manifest.identifiers.push(Identifier {
        raw: id(2),
        r#type: ACCOUNT,
    });
    assert_eq!(
        validate(&case.fetched, &plan),
        Err(Refusal::TwoAccountRecords)
    );
}

#[test]
fn validate_refuses_an_insert_that_is_not_listed() {
    let (case, mut plan) = sound();
    plan.manifest.identifiers.retain(|i| i.raw != id(0xA1));
    assert_eq!(
        validate(&case.fetched, &plan),
        Err(Refusal::InsertNotListed)
    );
}

#[test]
fn validate_refuses_a_delete_that_is_still_listed() {
    let (case, mut plan) = sound();
    plan.manifest.identifiers.push(Identifier {
        raw: id(1),
        r#type: CONTACT,
    });
    assert_eq!(validate(&case.fetched, &plan), Err(Refusal::DeleteListed));
}

#[test]
fn validate_refuses_a_version_other_than_remote_plus_one() {
    for version in [10, 12, 0, u64::MAX] {
        let (case, mut plan) = sound();
        plan.manifest.version = version;
        assert_eq!(
            validate(&case.fetched, &plan),
            Err(Refusal::Version),
            "version {version}"
        );
    }
    // A remote version that cannot be raised.
    let (mut case, _) = sound();
    case.fetched.version = u64::MAX;
    assert_eq!(case.run().unwrap_err(), Refusal::Version);
}

#[test]
fn validate_refuses_a_changed_record_ikm() {
    let (case, mut plan) = sound();
    plan.manifest.record_ikm = vec![8; 32];
    assert_eq!(validate(&case.fetched, &plan), Err(Refusal::RecordIkm));
    // Dropped.
    let (case, mut plan) = sound();
    plan.manifest.record_ikm = Vec::new();
    assert_eq!(validate(&case.fetched, &plan), Err(Refusal::RecordIkm));
    // Invented for a manifest that had none.
    let (mut case, _) = sound();
    case.fetched.record_ikm = Vec::new();
    let mut plan = case.plan();
    plan.manifest.record_ikm = vec![1; 32];
    assert_eq!(validate(&case.fetched, &plan), Err(Refusal::RecordIkm));
}

#[test]
fn validate_refuses_a_manifest_that_is_not_the_fetched_one_changed_by_the_plan() {
    // A kept identifier vanished (the force push's mark).
    let (case, mut plan) = sound();
    plan.manifest.identifiers.retain(|i| i.raw != id(2));
    assert_eq!(validate(&case.fetched, &plan), Err(Refusal::Drift));
    // One appeared.
    let (case, mut plan) = sound();
    plan.manifest.identifiers.push(Identifier {
        raw: id(0x66),
        r#type: STORY,
    });
    assert_eq!(validate(&case.fetched, &plan), Err(Refusal::Drift));
    // A kept identifier changed its type.
    let (case, mut plan) = sound();
    plan.manifest
        .identifiers
        .iter_mut()
        .find(|i| i.raw == id(2))
        .unwrap()
        .r#type = STICKER;
    assert_eq!(validate(&case.fetched, &plan), Err(Refusal::Drift));
    // A delete the phone never listed.
    let (case, mut plan) = sound();
    plan.deletes.push(id(0x55));
    assert_eq!(validate(&case.fetched, &plan), Err(Refusal::Drift));
}

#[test]
fn validate_refuses_an_id_that_is_not_16_bytes() {
    let (case, mut plan) = sound();
    plan.inserts[0].key = vec![0xA1; 15];
    plan.manifest
        .identifiers
        .iter_mut()
        .find(|i| i.raw == id(0xA1))
        .unwrap()
        .raw = vec![0xA1; 15];
    assert_eq!(validate(&case.fetched, &plan), Err(Refusal::BadId));
    // From the generator (16 bytes by its type): an id of zeros is still a sound id.
    let (case, _) = sound();
    assert!(case.run_with(&mut || [0; 16]).is_ok());
}

// ---------------------------------------------------------------------------------------------
// Pins
// ---------------------------------------------------------------------------------------------

#[test]
fn pins_keep_the_phones_entries_and_append_ours_in_seq_order() {
    // The phone's four entries: a person (old form), a group, the release notes, one that names
    // nothing this build knows.
    let before = account_rec(
        &[],
        &[
            old_contact_entry(2),
            group_entry(3),
            notes_entry(),
            opaque_entry(),
        ],
    );
    let case = Case::new(&[(1, ACCOUNT)], &[])
        .store(1, ACCOUNT_TARGET, before, 100, 1)
        .limit(6)
        .intents(vec![
            // The group was clicked after the person, but the order is the seq.
            pin(ACCOUNT_TARGET, &person(8), true, 7),
            pin(ACCOUNT_TARGET, &group_t(5), true, 4),
            pin(ACCOUNT_TARGET, &person(2), false, 9),
        ]);
    let plan = case.plan();
    let expected = account_rec(
        &[],
        &[
            group_entry(3),
            notes_entry(),
            opaque_entry(),
            new_group_entry(5),
            new_contact_entry(8),
        ],
    );
    assert_eq!(&plan.inserts[0].record[..], &expected[..]);
    assert!(plan.left.is_empty());
    assert_eq!(
        planned_view(&plan).iter().map(|p| p.1).collect::<Vec<_>>(),
        vec![4, 7, 9]
    );
    // A pin already on the phone is closed without a second entry.
    let case = Case::new(&[(1, ACCOUNT)], &[])
        .store(
            1,
            ACCOUNT_TARGET,
            account_rec(&[], &[old_contact_entry(2)]),
            100,
            1,
        )
        .intents(vec![pin(ACCOUNT_TARGET, &person(2), true, 3)]);
    let plan = case.plan();
    assert_eq!(
        &plan.inserts[0].record[..],
        &account_rec(&[], &[old_contact_entry(2)])[..]
    );
    assert_eq!(planned_view(&plan).len(), 1);
}

#[test]
fn a_pin_rebase_over_the_limit_drops_our_newest_addition() {
    // The phone pinned three chats since we clicked; we added A, B and C (seq 1, 2, 3). The
    // limit is 4: A stays, B and C go (newest first).
    let before = account_rec(&[], &[group_entry(1), group_entry(2), group_entry(3)]);
    let case = Case::new(&[(1, ACCOUNT)], &[])
        .store(1, ACCOUNT_TARGET, before.clone(), 100, 1)
        .intents(vec![
            pin(ACCOUNT_TARGET, &person(11), true, 1),
            pin(ACCOUNT_TARGET, &person(12), true, 2),
            pin(ACCOUNT_TARGET, &person(13), true, 3),
        ]);
    let plan = case.plan();
    assert_eq!(
        &plan.inserts[0].record[..],
        &account_rec(
            &[],
            &[
                group_entry(1),
                group_entry(2),
                group_entry(3),
                new_contact_entry(11)
            ]
        )[..]
    );
    assert_eq!(planned_view(&plan).len(), 1);
    assert_eq!(planned_view(&plan)[0].1, 1);
    let left: Vec<(i64, Why)> = plan.left.iter().map(|l| (l.seq, l.why)).collect();
    assert_eq!(left, vec![(3, Why::OverPinLimit), (2, Why::OverPinLimit)]);

    // A removal of ours makes room: the limit counts what is written.
    let case = Case::new(&[(1, ACCOUNT)], &[])
        .store(1, ACCOUNT_TARGET, before.clone(), 100, 1)
        .intents(vec![
            pin(ACCOUNT_TARGET, &group_t(1), false, 1),
            pin(ACCOUNT_TARGET, &person(11), true, 2),
            pin(ACCOUNT_TARGET, &person(12), true, 3),
            pin(ACCOUNT_TARGET, &person(13), true, 4),
        ]);
    let plan = case.plan();
    assert_eq!(
        &plan.inserts[0].record[..],
        &account_rec(
            &[],
            &[
                group_entry(2),
                group_entry(3),
                new_contact_entry(11),
                new_contact_entry(12)
            ]
        )[..]
    );
    assert_eq!(
        plan.left.iter().map(|l| (l.seq, l.why)).collect::<Vec<_>>(),
        vec![(4, Why::OverPinLimit)]
    );

    // The phone already passes the limit: none of ours is added and, with nothing else to say,
    // there is no write. The phone's entries are never touched.
    let case = Case::new(&[(1, ACCOUNT)], &[])
        .store(
            1,
            ACCOUNT_TARGET,
            account_rec(
                &[],
                &[
                    group_entry(1),
                    group_entry(2),
                    group_entry(3),
                    group_entry(4),
                    group_entry(5),
                ],
            ),
            100,
            1,
        )
        .intents(vec![pin(ACCOUNT_TARGET, &person(11), true, 1)]);
    let plan = case.plan();
    assert!(plan.is_empty());
    assert!(plan.deletes.is_empty());
    assert_eq!(plan.left.len(), 1);
    assert_eq!(plan.left[0].why, Why::OverPinLimit);
}

// ---------------------------------------------------------------------------------------------
// The safety number (M2), what cannot be written, a record that does not decode
// ---------------------------------------------------------------------------------------------

#[test]
fn an_identity_state_is_written_only_for_the_intents_key() {
    let c = person(1);
    let key = vec![5u8; 33];
    let with_key = contact_rec(1, &lenf(4, &key));
    let mut verified = intent(&c, intents::IDENTITY, Some(1), 1);
    verified.key = Some(key.clone());
    let case = Case::new(&[(1, CONTACT)], &[])
        .store(1, &c, with_key.clone(), 100, 1)
        .intents(vec![verified.clone()]);
    let plan = case.plan();
    // identityState (5) goes after the key (4); the key itself is untouched.
    assert_eq!(
        &plan.inserts[0].record[..],
        &contact_rec(1, &cat(&[&lenf(4, &key), &vint(5, 1)]))[..]
    );
    assert_eq!(
        planned_view(&plan),
        vec![("identity".to_owned(), 1, Some(1))]
    );

    // Back to the default: the field is gone.
    let mut cleared = verified.clone();
    cleared.value = Some(0);
    let case = Case::new(&[(1, CONTACT)], &[])
        .store(
            1,
            &c,
            contact_rec(1, &cat(&[&lenf(4, &key), &vint(5, 1)])),
            100,
            1,
        )
        .intents(vec![cleared]);
    assert_eq!(
        &case.plan().inserts[0].record[..],
        &contact_rec(1, &lenf(4, &key))[..]
    );

    // Another key, no key in the intent, no key in the record: not written, dropped as OtherKey.
    let mut other = verified.clone();
    other.key = Some(vec![6u8; 33]);
    let mut keyless = verified.clone();
    keyless.key = None;
    for (record, wanted) in [
        (with_key.clone(), other),
        (with_key.clone(), keyless),
        (contact_rec(1, &[]), verified.clone()),
    ] {
        let case = Case::new(&[(1, CONTACT)], &[])
            .store(1, &c, record, 100, 1)
            .intents(vec![wanted]);
        let plan = case.plan();
        assert!(plan.is_empty());
        assert!(plan.deletes.is_empty());
        assert_eq!(plan.left.len(), 1);
        assert_eq!(plan.left[0].why, Why::OtherKey);
    }
}

#[test]
fn what_a_record_cannot_take_is_left_and_the_rest_is_written() {
    let (a, g) = (ACCOUNT_TARGET, group_t(5));
    // Note to Self has no mute, a group has no safety number, a name this build does not know,
    // a pin of a person known only by a PNI.
    let pni = ServiceId::from(libsignal_core::Pni::from(Uuid::from_u128(77))).service_id_string();
    let case = Case::new(&[(1, ACCOUNT), (2, GROUP)], &[])
        .store(1, a, account_rec(&[], &[]), 100, 1)
        .store(2, &g, group_rec(5, &[]), 100, 2)
        .intents(vec![
            intent(a, intents::ARCHIVED, Some(1), 1),
            intent(a, intents::MUTED_UNTIL, Some(5), 2),
            intent(a, "future_field", Some(1), 3),
            pin(a, &format!("contact:{pni}"), true, 4),
            intent(&g, intents::IDENTITY, Some(1), 5),
        ]);
    let plan = case.plan();
    // The account record is rotated for the archive only; the group has nothing to write.
    assert_eq!(plan.inserts.len(), 1);
    assert_eq!(
        &plan.inserts[0].record[..],
        &account_rec(&vint(5, 1), &[])[..]
    );
    assert_eq!(plan.deletes, vec![id(1)]);
    let mut left: Vec<(i64, Why)> = plan.left.iter().map(|l| (l.seq, l.why)).collect();
    left.sort_by_key(|l| l.0);
    assert_eq!(
        left,
        vec![
            (2, Why::NotWritable),
            (3, Why::NotWritable),
            (4, Why::NotWritable),
            (5, Why::NotWritable)
        ]
    );
    assert_eq!(
        planned_view(&plan),
        vec![("archived".to_owned(), 1, Some(1))]
    );
}

#[test]
fn a_record_that_does_not_decode_is_never_edited() {
    let (c, g) = (person(1), group_t(5));
    // The contact's stored bytes are cut off; the group is fine. The contact keeps its intent
    // and its record, the group is written.
    let broken = lenf(1, b"\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff");
    let case = Case::new(&[(1, CONTACT), (2, GROUP)], &[])
        .store(1, &c, broken, 100, 1)
        .store(2, &g, group_rec(5, &[]), 100, 2)
        .intents(vec![
            intent(&c, intents::ARCHIVED, Some(1), 1),
            intent(&g, intents::ARCHIVED, Some(1), 2),
        ]);
    let plan = case.plan();
    assert_eq!(plan.inserts.len(), 1);
    assert_eq!(plan.inserts[0].target, g);
    assert_eq!(plan.deletes, vec![id(2)]);
    assert!(listed(&plan).contains(&(id(1), CONTACT)));
    assert_eq!(
        plan.left.iter().map(|l| (l.seq, l.why)).collect::<Vec<_>>(),
        vec![(1, Why::Unreadable)]
    );
    // Duplicates of an unreadable target are not deleted either.
    let case = Case::new(&[(1, CONTACT), (3, CONTACT)], &[])
        .store(1, &c, lenf(1, &[0xff; 3]), 200, 2)
        .store(3, &c, contact_rec(1, &[]), 100, 1)
        .intents(vec![intent(&c, intents::ARCHIVED, Some(1), 1)]);
    let plan = case.plan();
    assert!(plan.is_empty());
    assert!(plan.deletes.is_empty());
}

#[test]
fn a_target_the_manifest_does_not_list_is_left() {
    let c = person(1);
    // The row is stored but the fetched manifest no longer lists it: nothing to rotate, nothing
    // to delete, the intent stays for the merge to decide.
    let case = Case::new(&[(2, STORY)], &[])
        .store(1, &c, contact_rec(1, &[]), 100, 1)
        .intents(vec![intent(&c, intents::ARCHIVED, Some(1), 1)]);
    let plan = case.plan();
    assert!(plan.is_empty());
    assert!(plan.deletes.is_empty());
    assert_eq!(plan.left.len(), 1);
    assert_eq!(plan.left[0].why, Why::NotListed);
    // And a target no record kind is known for.
    let case = Case::new(&[(2, STORY)], &[]).intents(vec![intent(
        "story:1",
        intents::ARCHIVED,
        Some(1),
        1,
    )]);
    assert_eq!(case.plan().left[0].why, Why::NotWritable);
}

// ---------------------------------------------------------------------------------------------
// The in-flight row, log safety, the database inputs
// ---------------------------------------------------------------------------------------------

#[test]
fn the_planned_list_is_the_json_the_in_flight_row_reads_back() {
    let c = person(1);
    let case = Case::new(&[(1, CONTACT)], &[])
        .store(1, &c, contact_rec(1, &[]), 100, 1)
        .intents(vec![
            intent(&c, intents::ARCHIVED, Some(1), 3),
            intent(&c, intents::MUTED_UNTIL, None, 4),
            intent(&c, intents::MARKED_UNREAD, Some(0), 5),
        ]);
    let plan = case.plan();
    let json = plan.planned_json().unwrap();
    // Integers and null, never booleans or strings (what `intents` reads). `prior` (the intents'
    // base, review N1 (b)) is new in round 2: this JSON changed with it.
    assert_eq!(
        json,
        format!(
            r#"[{{"target":"{c}","field":"archived","value":1,"seq":3,"prior":0}},{{"target":"{c}","field":"muted_until","value":null,"seq":4,"prior":0}},{{"target":"{c}","field":"marked_unread","value":0,"seq":5,"prior":0}}]"#
        )
    );
    // The intents module reads it back from the row as the same list.
    let f = fixture();
    f.db.zone(|tx| -> Result<()> {
        tx.execute(
            "INSERT INTO storage_write_inflight(id, version, new_keys, planned, generation, planned_at)
             VALUES (1, 11, x'', ?1, 0, 0)",
            [&json],
        )?;
        Ok(())
    })
    .unwrap();
    let read = f.db.read(intents::planned).unwrap();
    assert_eq!(read, plan.planned);
}

#[test]
fn a_plan_prints_no_chat_and_no_byte() {
    let c = person(1);
    let case = Case::new(&[(1, CONTACT)], &[7; 32])
        .store(1, &c, contact_rec(1, &[]), 100, 1)
        .intents(vec![
            intent(&c, intents::ARCHIVED, Some(1), 1),
            intent(&c, "future_field", Some(1), 2),
        ]);
    let plan = case.plan();
    let shown = format!(
        "{plan:?} {:?} {:?} {:?} {:?}",
        plan.inserts[0], plan.left, plan.planned, case.stored[0]
    );
    for secret in [sid(1), c.clone(), "contact:".to_owned(), "Given".to_owned()] {
        assert!(!shown.contains(&secret), "{shown}");
    }
    assert!(shown.contains("inserts: 1"), "{shown}");
    assert!(!shown.contains("[161"), "{shown}");
}

#[test]
fn the_database_inputs_are_the_rows_of_the_touched_targets() {
    let (c, g) = (person(1), group_t(5));
    let intents = vec![
        intent(&g, intents::ARCHIVED, Some(1), 1),
        intent(&c, intents::ARCHIVED, Some(1), 2),
        intent(&c, intents::MUTED_UNTIL, None, 3),
    ];
    // Distinct, in a fixed order.
    assert_eq!(touched_targets(&intents), vec![c.clone(), g.clone()]);

    let f = fixture();
    f.db.zone(|tx| -> Result<()> {
        for (key, target, peer, updated_at) in [
            (1u8, &c, Some(sid(1)), 100i64),
            (2, &c, Some(sid(1)), 300),
            (3, &g, None, 100),
            (4, &person(4), Some(sid(4)), 100),
        ] {
            tx.execute(
                "INSERT INTO storage_records(key, kind, peer, record, updated_at, target)
                 VALUES (?1, 1, ?2, ?3, ?4, ?5)",
                (id(key), peer, contact_rec(1, &[]), updated_at, target),
            )?;
        }
        Ok(())
    })
    .unwrap();
    let stored =
        f.db.read(|conn| load_stored(conn, &touched_targets(&intents)))
            .unwrap();
    let mut seen: Vec<(Vec<u8>, String, Option<String>, i64)> = stored
        .iter()
        .map(|s| {
            (
                s.key.clone(),
                s.target.clone(),
                s.peer.clone(),
                s.updated_at,
            )
        })
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        vec![
            (id(1), c.clone(), Some(sid(1)), 100),
            (id(2), c.clone(), Some(sid(1)), 300),
            (id(3), g.clone(), None, 100),
        ],
        "the row of another target is not read"
    );
    assert!(stored.iter().all(|s| s.rowid > 0));
    assert_eq!(&stored[0].raw[..], &contact_rec(1, &[])[..]);
    // New identifiers are 16 random bytes.
    assert_ne!(random_record_id(), random_record_id());
}

#[test]
fn the_told_bases_come_from_the_peer_the_group_and_note_to_self() {
    let f = fixture();
    let wake = Rc::new(Notify::new());
    let own = ServiceId::from(f.account.aci).service_id_string();
    // A group the phone listed (archived), merged as a sync does: it gets its row.
    let record = proto::StorageRecord {
        record: Some(Record::GroupV2(proto::GroupV2Record {
            master_key: vec![5; 32],
            archived: true,
            ..Default::default()
        })),
    };
    let items = vec![Decrypted {
        key: id(9),
        kind: GROUP,
        raw: Zeroizing::new(record.encode_to_vec()),
        record,
    }];
    f.db.zone(|tx| merge_zone(tx, &f.account, &items, None, false, 10, &wake))
        .unwrap();
    f.db.zone(|tx| -> Result<()> {
        crate::peers::set_roaming_archived(tx, &sid(1), true, 11)?;
        crate::peers::set_roaming_archived(tx, &own, true, 11)?;
        Ok(())
    })
    .unwrap();
    let targets = vec![
        person(1),
        // No row of the core: the base is learned from the (absent) record: not archived.
        person(2),
        group_t(5),
        // A group the core does not have: no base, the plan keeps the record's flag.
        group_t(6),
        ACCOUNT_TARGET.to_owned(),
        // Not a target at all.
        "story:1".to_owned(),
    ];
    let told =
        f.db.zone(|tx| intents::told_archived(tx, &f.account, &targets, 20))
            .unwrap();
    assert_eq!(told.get(&person(1)), Some(&true));
    assert_eq!(told.get(&person(2)), Some(&false));
    assert_eq!(told.get(&group_t(5)), Some(&true));
    assert_eq!(told.get(&group_t(6)), None);
    assert_eq!(told.get(ACCOUNT_TARGET), Some(&true));
    assert_eq!(told.len(), 4);
}

// ---------------------------------------------------------------------------------------------
// The write in flight and the landing rule (B6; §3.6, the lead's BQ9)
// ---------------------------------------------------------------------------------------------

/// A write in flight that set version 8 with the new identifiers `[0xa1; 16]` and `[0xa2; 16]`.
fn row() -> InFlight {
    InFlight {
        version: 8,
        new_keys: vec![id(0xa1), id(0xa2)],
        planned: vec![Planned {
            target: person(1),
            field: ARCHIVED.to_owned(),
            value: Some(1),
            seq: 4,
            rides: false,
            overtaken: false,
            prior: None,
            state_at_landing: false,
        }],
        generation: 3,
        planned_at: 1_000,
    }
}

/// The identifiers of the fetched manifest: `listed` of ours and two of the phone's.
fn fetched_at(version: u64, listed: &[u8]) -> proto::ManifestRecord {
    let mut ids: Vec<(u8, i32)> = vec![(1, CONTACT), (2, ACCOUNT)];
    ids.extend(listed.iter().map(|&n| (n, CONTACT)));
    manifest(version, &ids, &[9; 32])
}

#[test]
fn an_older_version_is_not_landed() {
    // Even when one of our identifiers is listed (which a server behind our version cannot do):
    // the version decides first.
    for listed in [&[][..], &[0xa1][..], &[0xa1, 0xa2][..]] {
        assert_eq!(landed(&fetched_at(7, listed), &row(), &[]), Landed::No);
    }
    assert_eq!(landed(&fetched_at(0, &[]), &row(), &[]), Landed::No);
}

#[test]
fn the_same_version_with_one_of_our_ids_is_landed() {
    // One of the two is enough, and either.
    for listed in [&[0xa1][..], &[0xa2][..], &[0xa1, 0xa2][..]] {
        assert_eq!(landed(&fetched_at(8, listed), &row(), &[]), Landed::Yes);
    }
}

/// BQ9's missing row: the phone wrote the version we meant to write, our records are not in it,
/// so our write did NOT land and goes again.
#[test]
fn the_same_version_without_our_ids_is_not_landed() {
    assert_eq!(landed(&fetched_at(8, &[]), &row(), &[]), Landed::No);
    // An identifier that only looks like ours (one byte off) is not ours.
    let mut near = id(0xa1);
    near[15] = 0xa0;
    let mut fetched = fetched_at(8, &[]);
    fetched.identifiers.push(Identifier {
        raw: near,
        r#type: CONTACT,
    });
    assert_eq!(landed(&fetched, &row(), &[]), Landed::No);
    assert!(!Landed::No.is_landed());
}

#[test]
fn a_moved_version_with_one_of_our_ids_is_landed() {
    for version in [9, 10, 1_000, u64::MAX] {
        assert_eq!(
            landed(&fetched_at(version, &[0xa2]), &row(), &[]),
            Landed::Yes
        );
    }
}

/// BQ9: the phone wrote again before we looked and replaced our records: counted as landed (a
/// lost click is preferred to undoing the phone). The caller logs a count.
#[test]
fn a_moved_version_without_our_ids_counts_as_landed() {
    for version in [9, 10, u64::MAX] {
        let verdict = landed(&fetched_at(version, &[]), &row(), &[]);
        assert_eq!(verdict, Landed::Presumed, "version {version}");
        assert!(verdict.is_landed());
    }
    // A row with no new identifier at all can never be seen in a manifest: the version decides.
    let mut empty = row();
    empty.new_keys.clear();
    assert_eq!(landed(&fetched_at(8, &[0xa1]), &empty, &[]), Landed::No);
    assert_eq!(
        landed(&fetched_at(9, &[0xa1]), &empty, &[]),
        Landed::Presumed
    );
}

/// D1: the manifest moved past our version and lists none of our ids, but one our write DELETED is
/// still listed: a Storage Service write is atomic, so ours was not applied (or the phone put its
/// own record back under the old id). NOT landed, written again. A deleted id that is not listed
/// (the phone replaced it) leaves the presumption as it was.
#[test]
fn a_moved_version_that_still_lists_a_deleted_id_is_not_landed() {
    // `fetched_at` lists ids 1 and 2 (the phone's) and the given ones.
    for version in [9, 10, u64::MAX] {
        let fetched = fetched_at(version, &[]);
        assert_eq!(
            landed(&fetched, &row(), &[id(1)]),
            Landed::No,
            "version {version}: the record our write replaced is still there"
        );
        assert_eq!(landed(&fetched, &row(), &[id(0x77), id(2)]), Landed::No);
        assert_eq!(
            landed(&fetched, &row(), &[id(0x77)]),
            Landed::Presumed,
            "version {version}: a deleted id the phone replaced too"
        );
    }
    // One of our ids listed decides first, whatever else is still listed.
    assert_eq!(
        landed(&fetched_at(9, &[0xa1]), &row(), &[id(1)]),
        Landed::Yes
    );
    // At the write's own version the answer is No with or without the check.
    assert_eq!(landed(&fetched_at(8, &[]), &row(), &[id(0x77)]), Landed::No);
}

/// D1: the ids the write deleted are the stored records of the carried targets that are not newer
/// than the row: a record merged after the plan is the phone's, a record of another target is not
/// carried.
#[test]
fn the_deleted_ids_are_the_older_stored_records_of_the_carried_targets() {
    let f = fixture();
    let (carried, other) = (person(1), person(4));
    f.db.zone(|tx| -> Result<()> {
        for (key, target, updated_at) in [
            (1u8, &carried, 100i64),
            (2, &carried, 1_000),
            (3, &carried, 1_001),
            (4, &other, 100),
        ] {
            tx.execute(
                "INSERT INTO storage_records(key, kind, peer, record, updated_at, target)
                 VALUES (?1, 1, NULL, ?2, ?3, ?4)",
                (id(key), contact_rec(1, &[]), updated_at, target),
            )?;
        }
        Ok(())
    })
    .unwrap();
    // `row()` carries `person(1)` and was planned at 1_000.
    let mut deleted = f.db.read(|c| deleted_ids(c, &row())).unwrap();
    deleted.sort();
    assert_eq!(
        deleted,
        [id(1), id(2)],
        "100 and 1_000, not 1_001, not the other target"
    );
    let mut none = row();
    none.planned.clear();
    assert!(f.db.read(|c| deleted_ids(c, &none)).unwrap().is_empty());
}

#[test]
fn the_in_flight_row_is_written_read_and_deleted() {
    let f = fixture();
    assert_eq!(f.db.read(read_in_flight).unwrap(), None, "none at first");
    let written = row();
    f.db.zone(|tx| write_in_flight(tx, &written)).unwrap();
    let read = f.db.read(read_in_flight).unwrap().expect("the row");
    assert_eq!(
        read, written,
        "every column comes back, the ids as 16-byte keys"
    );
    // The ids are stored back to back (B1's column), the list as the JSON `intents` reads.
    let (keys, planned): (Vec<u8>, String) =
        f.db.read(|c| -> Result<_> {
            Ok(c.query_row(
                "SELECT new_keys, planned FROM storage_write_inflight WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!(keys, [id(0xa1), id(0xa2)].concat());
    assert_eq!(
        planned,
        format!(
            r#"[{{"target":"{}","field":"archived","value":1,"seq":4}}]"#,
            person(1)
        )
    );
    assert_eq!(f.db.read(intents::planned).unwrap(), written.planned);
    // A second row is refused, not replaced: the first may stand for a write that landed.
    let mut other = row();
    other.version = 9;
    assert!(f.db.zone(|tx| write_in_flight(tx, &other)).is_err());
    assert_eq!(f.db.read(read_in_flight).unwrap(), Some(written));
    // Deleted; deleting again is no error.
    f.db.zone(|tx| delete_in_flight(tx)).unwrap();
    f.db.zone(|tx| delete_in_flight(tx)).unwrap();
    assert_eq!(f.db.read(read_in_flight).unwrap(), None);
    // An unmute is a null value and comes back as one.
    let mut unmute = row();
    unmute.planned[0].field = MUTED_UNTIL.to_owned();
    unmute.planned[0].value = None;
    f.db.zone(|tx| write_in_flight(tx, &unmute)).unwrap();
    assert_eq!(f.db.read(read_in_flight).unwrap(), Some(unmute));
}

/// The row of a real plan: the version, the new identifiers (in manifest order) and the planned
/// list of the plan, under the generation it was planned in.
#[test]
fn the_row_of_a_plan_carries_its_version_ids_and_list() {
    let f = fixture();
    let target = person(1);
    let mut plan = Case::new(&[(1, CONTACT), (2, ACCOUNT)], &[9; 32])
        .store(1, &target, contact_rec(1, &[]), 100, 1)
        .intents(vec![intent(&target, intents::ARCHIVED, Some(1), 3)])
        .plan();
    let row = InFlight::of(&plan, 5, 777);
    assert_eq!(row.version, plan.manifest.version);
    assert_eq!(
        row.new_keys,
        plan.inserts
            .iter()
            .map(|i| i.key.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(row.planned, plan.planned);
    assert_eq!((row.generation, row.planned_at), (5, 777));
    assert!(!row.new_keys.is_empty());
    // The plan's own JSON is what the row stores.
    f.db.zone(|tx| write_in_flight(tx, &row)).unwrap();
    let json: String =
        f.db.read(|c| -> Result<_> {
            Ok(
                c.query_row("SELECT planned FROM storage_write_inflight", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .unwrap();
    assert_eq!(json, plan.planned_json().unwrap());
    // The manifest the plan sends lists the ids the row remembers: the landing rule sees them.
    assert_eq!(landed(&plan.manifest, &row, &[]), Landed::Yes);
    // And it is not landed at the version before.
    plan.manifest.version -= 1;
    assert_eq!(landed(&plan.manifest, &row, &[]), Landed::No);
}
