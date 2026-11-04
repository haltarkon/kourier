//! A10c chunk C1: the actor's switch and its route to the Storage Service write engine. The
//! four roaming changes of a chat become intents in ONE zone (the reply answers after its commit),
//! the durable clear is filed and confirmed in one zone, `SetRoamingWrites` states the caps with
//! BOTH facts (the flag and the pin limit), and a safety-number mark files its intent instead of
//! queueing a Verified sync while the switch is on. Over the in-memory fixture of the receive
//! tests, no network; synthetic ids only.

use kourier_model::{CmdId, ConvKey, ConversationChange, ErrorKind};
use libsignal_core::{Aci, ServiceId};
use libsignal_protocol::IdentityKey;
use prost::Message as _;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::*;
use crate::jobs::JobKind;
use crate::proto::manifest_record::identifier::Type as ItemType;
use crate::proto::{self, storage_record::Record};
use crate::receive::tests::{Fixture, fixture};
use crate::service_id::aci_user_id;
use crate::storage::intents;

/// libsignal's `BOB_IDENTITY` test vector: a valid serialized identity key.
const BOB_IDENTITY: &str = "05f781b6fb32fed9ba1cf2de978d4d5da28dc34046ae814402b5c0dbd96fda907b";

fn aci(n: u128) -> Aci {
    Aci::from(Uuid::from_u128(n))
}

fn sid(n: u128) -> String {
    ServiceId::from(aci(n)).service_id_string()
}

fn chat(n: u128) -> ConvKey {
    ConvKey::Direct(aci_user_id(aci(n)).unwrap())
}

fn bob_key() -> IdentityKey {
    let bytes: Vec<u8> = (0..BOB_IDENTITY.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&BOB_IDENTITY[i..i + 2], 16).unwrap())
        .collect();
    IdentityKey::decode(&bytes).unwrap()
}

/// A hand-built actor over the receive fixture's `backend.db`, linked, switch OFF (the default
/// of every account), with nothing queued for the core.
struct World {
    actor: Actor<testing::NoTransport>,
    fixture: Fixture,
}

fn world() -> World {
    let fixture = fixture();
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = Rc::new(test.shared);
    let (_commands_tx, commands) = mpsc::channel(8);
    let actor = Actor {
        shared,
        commands,
        cancel: CancellationToken::new(),
        login: None,
        connection: None,
        held: false,
        transfer: None,
        scrub_owed: false,
        forwarder: spawn_local(async {}),
        forwarder_stop: CancellationToken::new(),
        stage2: spawn_local(async {}),
        stage2_stop: CancellationToken::new(),
    };
    let world = World { actor, fixture };
    world.sql("DELETE FROM core_outbox");
    world.sql("DELETE FROM jobs");
    world
}

impl World {
    fn sql(&self, sql: &str) {
        self.fixture
            .db
            .zone(|tx| -> crate::error::Result<()> {
                tx.execute(sql, [])?;
                Ok(())
            })
            .unwrap();
    }

    /// Sets the stored switch without a command (a test's starting point).
    fn switch_is(&self, on: bool) {
        self.sql(&format!(
            "UPDATE account SET roaming_writes = {}",
            i32::from(on)
        ));
    }

    fn switch(&self) -> bool {
        self.fixture.db.read(intents::switch_is_on).unwrap()
    }

    /// A stored Storage Service record the phone wrote, as a sync leaves it.
    fn store(&self, key: u8, kind: ItemType, peer: Option<&str>, record: Record) {
        let bytes = proto::StorageRecord {
            record: Some(record),
        }
        .encode_to_vec();
        self.fixture
            .db
            .zone(|tx| -> crate::error::Result<()> {
                tx.execute(
                    "INSERT INTO storage_records(key, kind, peer, record, updated_at)
                     VALUES (?1, ?2, ?3, ?4, 50)",
                    (&[key; 16][..], kind as i32, peer, &bytes),
                )?;
                if let Some(peer) = peer {
                    crate::peers::ensure(tx, peer, 1)?;
                }
                Ok(())
            })
            .unwrap();
    }

    /// The phone lists person `n`, with `edit` applied to its record.
    fn phone_lists(&self, n: u128, edit: impl FnOnce(&mut proto::ContactRecord)) {
        let mut record = proto::ContactRecord {
            aci_binary: Uuid::from_u128(n).as_bytes().to_vec(),
            ..Default::default()
        };
        edit(&mut record);
        self.store(
            n as u8,
            ItemType::Contact,
            Some(&sid(n)),
            Record::Contact(record),
        );
    }

    /// The phone's account record (the pin list lives in it).
    fn phone_has_an_account_record(&self) {
        self.store(
            0xac,
            ItemType::Account,
            None,
            Record::Account(proto::AccountRecord::default()),
        );
    }

    /// `Command::ChangeConversation`, answered.
    async fn change(
        &mut self,
        conv: ConvKey,
        change: ConversationChange,
    ) -> std::result::Result<(), BackendError> {
        let (reply, answer) = oneshot::channel();
        self.actor
            .command(Command::ChangeConversation {
                conv,
                change,
                id: None,
                reply,
            })
            .await;
        answer.await.expect("the actor answered")
    }

    /// `Command::SetRoamingWrites`, answered.
    async fn set_switch(&mut self, on: bool) -> std::result::Result<(), BackendError> {
        let (reply, answer) = oneshot::channel();
        self.actor
            .command(Command::SetRoamingWrites { on, reply })
            .await;
        answer.await.expect("the actor answered")
    }

    /// `Command::ChangeConversationDurable`, which has no reply: the core hears
    /// `CommandPersisted`.
    async fn durable_clear(&mut self, cmd: i64, conv: ConvKey) {
        self.actor
            .command(Command::ChangeConversationDurable {
                cmd: CmdId(cmd),
                conv,
                change: ConversationChange::SetMarkedUnread {
                    marked_unread: false,
                },
                asked_at: 1_000,
            })
            .await;
    }

    /// `Command::SetVerified`, answered.
    async fn mark(&mut self, n: u128, verified: bool) -> std::result::Result<(), BackendError> {
        let (reply, answer) = oneshot::channel();
        self.actor
            .command(Command::SetVerified {
                user: aci_user_id(aci(n)).unwrap(),
                verified,
                key: kourier_model::IdentityFingerprint(bob_key().serialize().to_vec()),
                reply,
            })
            .await;
        answer.await.expect("the actor answered")
    }

    /// The person `n` has a stored identity key (state DEFAULT), as a chat with them left it.
    fn knows_the_key_of(&self, n: u128) {
        self.fixture
            .db
            .zone(|tx| -> crate::error::Result<()> {
                tx.execute(
                    "INSERT INTO identities(service_id, identity_key, verified, first_use, updated_at)
                     VALUES (?1, ?2, 0, 0, 1)",
                    (sid(n), &bob_key().serialize()[..]),
                )?;
                Ok(())
            })
            .unwrap();
    }

    /// `(target, field, value, automatic)` of every stored intent, oldest first.
    fn intents(&self) -> Vec<(String, String, Option<i64>, bool)> {
        self.fixture
            .db
            .read(intents::all_intents)
            .unwrap()
            .into_iter()
            .map(|i| (i.target, i.field, i.value, i.automatic))
            .collect()
    }

    /// The payload of the queued job of `kind` for the key `key`, as JSON.
    fn payload_of(&self, kind: JobKind, key: &str) -> serde_json::Value {
        let bytes: Vec<u8> = self
            .fixture
            .db
            .read(|c| -> crate::error::Result<Vec<u8>> {
                Ok(c.query_row(
                    "SELECT payload FROM jobs WHERE kind = ?1 AND key = ?2",
                    (kind.as_str(), key),
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn jobs_of(&self, kind: JobKind) -> i64 {
        self.fixture
            .db
            .read(|c| crate::jobs::count(c, kind))
            .unwrap()
    }

    /// What the actor told the core, in order.
    fn events(&self) -> Vec<BackendEvent> {
        self.fixture
            .db
            .read(|c| -> crate::error::Result<Vec<BackendEvent>> {
                let mut stmt = c.prepare("SELECT envelope FROM core_outbox ORDER BY seq")?;
                let rows: Vec<Vec<u8>> = stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<kourier_db::rusqlite::Result<_>>()?;
                let mut events = Vec::new();
                for row in rows {
                    events.extend(kourier_backend_api::decode_outbox_row(&row)?);
                }
                Ok(events)
            })
            .unwrap()
    }

    /// The caps the actor stated last.
    fn last_caps(&self) -> Option<kourier_model::AccountCaps> {
        self.events().into_iter().rev().find_map(|e| match e {
            BackendEvent::AccountCaps(caps) => Some(caps),
            _ => None,
        })
    }
}

/// The four roaming changes with the field each files and its value.
fn four_changes() -> [(ConversationChange, &'static str, Option<i64>); 4] {
    [
        (
            ConversationChange::SetArchived { archived: true },
            intents::ARCHIVED,
            Some(1),
        ),
        (
            ConversationChange::SetMuted { until: Some(5_000) },
            intents::MUTED_UNTIL,
            Some(5_000),
        ),
        (
            ConversationChange::SetMarkedUnread {
                marked_unread: true,
            },
            intents::MARKED_UNREAD,
            Some(1),
        ),
        (
            ConversationChange::SetPinned { pinned: true },
            // The pin list is the account record's: one intent per chat on it.
            "pin:",
            Some(1),
        ),
    ]
}

/// With the switch on each of the four changes is answered `Ok` and stored as an intent of its
/// field (a pin on the account record), with the `storage_write` job queued and the core told.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_roaming_change_is_stored_as_an_intent_with_the_switch_on() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for (change, field, value) in four_changes() {
                let mut w = world();
                w.phone_lists(1, |_| {});
                w.phone_has_an_account_record();
                w.switch_is(true);
                w.change(chat(1), change.clone())
                    .await
                    .unwrap_or_else(|e| panic!("{change:?} was refused: {:?}", e.kind));
                let stored = w.intents();
                assert!(
                    stored.iter().any(|(target, f, v, automatic)| {
                        f.starts_with(field)
                            && *v == value
                            && !automatic
                            && if field == "pin:" {
                                target == "account"
                            } else {
                                *target == format!("contact:{}", sid(1))
                            }
                    }),
                    "{change:?} stored no intent of its field: {} intents",
                    stored.len()
                );
                assert_eq!(w.jobs_of(JobKind::StorageWrite), 1, "{change:?}");
                assert!(
                    w.events().iter().any(|e| matches!(
                        e,
                        BackendEvent::ConversationUpsert(_) | BackendEvent::PinnedConversations(_)
                    )),
                    "{change:?}: the core was not told"
                );
            }
        })
        .await;
}

/// With the switch off each of the four changes is refused with the kind the gate names and
/// NOTHING is stored: no intent, no job, no event for the core.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_roaming_change_is_refused_with_the_switch_off_and_stores_nothing() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for (change, ..) in four_changes() {
                let mut w = world();
                w.phone_lists(1, |_| {});
                w.phone_has_an_account_record();
                assert!(!w.switch(), "every account starts with the switch off");
                let error = w
                    .change(chat(1), change.clone())
                    .await
                    .expect_err("the switch is off");
                assert_eq!(error.kind, ErrorKind::Unsupported, "{change:?}");
                assert!(w.intents().is_empty(), "{change:?} stored an intent");
                assert_eq!(w.jobs_of(JobKind::StorageWrite), 0, "{change:?}");
                assert!(w.events().is_empty(), "{change:?} told the core");
            }
        })
        .await;
}

/// A refusal of the gate names its kind and stores nothing: a chat the phone does not list is
/// `InvalidInput`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chat_the_phone_does_not_list_is_refused_with_its_kind() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut w = world();
            w.switch_is(true);
            let error = w
                .change(chat(9), ConversationChange::SetArchived { archived: true })
                .await
                .expect_err("not listed");
            assert_eq!(error.kind, ErrorKind::InvalidInput);
            assert!(w.intents().is_empty());
            assert!(w.events().is_empty());
        })
        .await;
}

/// Block, accept a request and leave are no roaming settings: they stay refused as they were,
/// whatever the switch says, and nothing is stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_accept_and_leave_stay_refused_with_the_switch_on() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut w = world();
            w.phone_lists(1, |_| {});
            w.switch_is(true);
            for change in [
                ConversationChange::SetBlocked { blocked: true },
                ConversationChange::AcceptRequest,
                ConversationChange::Leave,
            ] {
                let error = w
                    .change(chat(1), change.clone())
                    .await
                    .expect_err("not written to the phone");
                assert_eq!(error.kind, ErrorKind::Unsupported, "{change:?}");
            }
            assert!(w.intents().is_empty());
            assert!(w.events().is_empty());
        })
        .await;
}

/// The core's durable clear of a mark is filed as an automatic intent AND confirmed
/// (`CommandPersisted`) by one command, with the switch on; with it off it is confirmed and
/// dropped (the core must not resend it at every start).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_durable_clear_is_filed_and_confirmed() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut w = world();
            w.phone_lists(1, |c| c.marked_unread = true);
            w.switch_is(true);
            w.durable_clear(7, chat(1)).await;
            assert_eq!(
                w.intents(),
                [(
                    format!("contact:{}", sid(1)),
                    intents::MARKED_UNREAD.to_owned(),
                    Some(0),
                    true
                )],
                "an automatic clear of the mark"
            );
            assert!(
                w.events().iter().any(
                    |e| matches!(e, BackendEvent::CommandPersisted { cmd } if *cmd == CmdId(7))
                ),
                "confirmed"
            );
            assert_eq!(w.jobs_of(JobKind::StorageWrite), 1, "the write is due");

            let mut off = world();
            off.phone_lists(1, |c| c.marked_unread = true);
            off.durable_clear(8, chat(1)).await;
            assert!(off.intents().is_empty(), "the switch is off: nothing filed");
            assert!(
                off.events().iter().any(
                    |e| matches!(e, BackendEvent::CommandPersisted { cmd } if *cmd == CmdId(8))
                ),
                "and still confirmed, so the core stops resending it"
            );
            assert_eq!(off.jobs_of(JobKind::StorageWrite), 0);
        })
        .await;
}

/// The intent and the confirmation are ONE zone: when the confirmation cannot be written the
/// intent is rolled back with it (a kill between two zones would otherwise leave a clear that
/// the core resends and files again).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_durable_clear_and_its_confirmation_commit_together() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut w = world();
            w.phone_lists(1, |c| c.marked_unread = true);
            w.switch_is(true);
            w.sql(
                "CREATE TRIGGER no_confirmation BEFORE INSERT ON core_outbox
                 BEGIN SELECT RAISE(ABORT, 'refused'); END",
            );
            w.durable_clear(7, chat(1)).await;
            assert!(
                w.intents().is_empty(),
                "the filing was committed without its confirmation"
            );
            assert_eq!(w.jobs_of(JobKind::StorageWrite), 0);
        })
        .await;
}

/// `SetRoamingWrites { on }` writes the column and states the caps again with BOTH facts: the
/// flag (`Some(true)`) and the pin limit (4 when the config states none, else its number).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_the_switch_on_states_the_flag_and_the_pin_limit_together() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut w = world();
            assert!(!w.switch());
            w.set_switch(true).await.expect("turned on");
            assert!(w.switch(), "the column");
            let caps = w.last_caps().expect("caps were stated");
            assert_eq!(caps.roaming_writes(), Some(true));
            assert_eq!(caps.max_pinned_chats, Some(4), "the default limit");

            w.sql("INSERT INTO remote_config(name, value) VALUES ('global.pinnedChatLimit', '6')");
            w.set_switch(true).await.expect("again");
            let caps = w.last_caps().expect("caps were stated");
            assert_eq!(caps.max_pinned_chats, Some(6), "the config's limit");
            assert_eq!(caps.roaming_writes(), Some(true));
        })
        .await;
}

/// Turning the switch off writes the column, drops the queued changes (the phone's values are
/// stated back to the core) and restates the caps with the flag `Some(false)` and the pin limit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_the_switch_off_drops_the_intents_and_states_the_caps_again() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut w = world();
            w.phone_lists(1, |_| {});
            w.set_switch(true).await.expect("on");
            w.change(chat(1), ConversationChange::SetArchived { archived: true })
                .await
                .expect("filed");
            assert_eq!(w.intents().len(), 1);
            w.sql("DELETE FROM core_outbox");

            w.set_switch(false).await.expect("off");
            assert!(!w.switch(), "the column");
            assert!(w.intents().is_empty(), "the queued change is dropped");
            let events = w.events();
            assert!(
                events.iter().any(|e| matches!(
                    e,
                    BackendEvent::ConversationUpsert(p) if p.archived == Some(false)
                )),
                "the phone's value is stated back to the core"
            );
            let caps = w.last_caps().expect("caps were stated");
            assert_eq!(caps.roaming_writes(), Some(false));
            assert_eq!(caps.max_pinned_chats, Some(4));
        })
        .await;
}

/// An account that is not linked has no switch to turn: refused, nothing stated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_switch_of_an_unlinked_account_is_refused() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut w = world();
            *w.actor.shared.account.borrow_mut() = None;
            let error = w.set_switch(true).await.expect_err("not linked");
            assert_eq!(error.kind, ErrorKind::LoggedOut);
            assert!(w.events().is_empty());
        })
        .await;
}

/// P5a with the switch on: a Mark files the `identity` intent in the same zone and queues NO
/// Verified sync (the state travels in the contact record); the core still hears the trust.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mark_files_the_identity_intent_and_queues_no_verified_sync_with_the_switch_on() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut w = world();
            w.knows_the_key_of(5);
            w.phone_lists(5, |c| c.identity_key = bob_key().serialize().to_vec());
            w.switch_is(true);
            w.mark(5, true).await.expect("marked");
            assert_eq!(
                w.intents(),
                [(
                    format!("contact:{}", sid(5)),
                    intents::IDENTITY.to_owned(),
                    Some(1),
                    false
                )],
                "the verified state, filed for the contact record"
            );
            assert_eq!(
                w.jobs_of(JobKind::VerifiedSync),
                0,
                "no Verified sync: the record carries the state"
            );
            assert_eq!(w.jobs_of(JobKind::StorageWrite), 1);
            assert!(
                w.events().iter().any(|e| matches!(
                    e,
                    BackendEvent::ContactUpsert(p)
                        if p.trust == Some(kourier_model::TrustLevel::Verified)
                )),
                "the core is told at once"
            );
        })
        .await;
}

/// P5a with the switch off behaves exactly as before: a Mark queues the Verified sync and files
/// no intent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mark_with_the_switch_off_queues_the_verified_sync_as_before() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut w = world();
            w.knows_the_key_of(5);
            w.phone_lists(5, |c| c.identity_key = bob_key().serialize().to_vec());
            w.mark(5, true).await.expect("marked");
            assert!(w.intents().is_empty(), "the switch is off: no intent");
            assert_eq!(w.jobs_of(JobKind::VerifiedSync), 1, "the sync, as before");
            assert_eq!(w.jobs_of(JobKind::StorageWrite), 0);
        })
        .await;
}

/// A person the phone has no contact record of cannot carry the state in one: with the switch on
/// the Verified sync stays the only route to the other devices (nothing he does today is lost).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_person_without_a_record_still_gets_the_verified_sync_with_the_switch_on() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut w = world();
            w.knows_the_key_of(5);
            w.switch_is(true);
            w.mark(5, true).await.expect("marked");
            assert!(w.intents().is_empty(), "no record to write");
            assert_eq!(w.jobs_of(JobKind::VerifiedSync), 1);
        })
        .await;
}

/// C5b (the lane session's call): the phone's record of the person holds ANOTHER key, or none.
/// The write puts `identityState` only onto a record of the same key, so the intent would be
/// dropped a second later (`Why::OtherKey`) and no other device would ever hear of the click: with
/// the switch on the intent is still filed (A10b's rule and base are unchanged) AND the Verified
/// sync is queued with the click's key and state, as with the switch off. (The same-key test above
/// keeps asserting no job.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_click_for_a_key_the_phones_record_does_not_hold_also_queues_the_verified_sync() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for (what, phones_key) in [("another key", vec![7u8; 33]), ("no key", Vec::new())] {
                let mut w = world();
                w.knows_the_key_of(5);
                w.phone_lists(5, |c| c.identity_key = phones_key);
                w.switch_is(true);
                w.mark(5, true).await.expect("marked");
                assert_eq!(
                    w.intents(),
                    [(
                        format!("contact:{}", sid(5)),
                        intents::IDENTITY.to_owned(),
                        Some(1),
                        false
                    )],
                    "{what}: the intent is filed"
                );
                let intent = w.fixture.db.read(intents::all_intents).unwrap().remove(0);
                assert_eq!(
                    intent.base,
                    Some(0),
                    "{what}: A10b's base, the state here before"
                );
                assert!(
                    intent.key.as_deref() == Some(&bob_key().serialize()[..]),
                    "{what}: for the key of the click"
                );
                assert_eq!(w.jobs_of(JobKind::VerifiedSync), 1, "{what}: the sync too");
                assert_eq!(w.jobs_of(JobKind::StorageWrite), 1, "{what}");
                let payload = w.payload_of(JobKind::VerifiedSync, &sid(5));
                assert_eq!(payload["state"], 1, "{what}: the click's state");
                assert!(
                    payload["key"] == serde_json::json!(bob_key().serialize().to_vec()),
                    "{what}: the click's key"
                );
            }
        })
        .await;
}

/// A Verified sync queued by a click made while the switch was off carries an OLDER state than
/// the intent a click makes once it is on: it goes, so it cannot tell the phone the old state
/// later nor hold back the phone's newer record (`Remote::Kept`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_intent_replaces_a_verified_sync_queued_while_the_switch_was_off() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut w = world();
            w.knows_the_key_of(5);
            w.phone_lists(5, |c| c.identity_key = bob_key().serialize().to_vec());
            w.mark(5, true).await.expect("marked while off");
            assert_eq!(w.jobs_of(JobKind::VerifiedSync), 1);
            w.set_switch(true).await.expect("on");
            w.mark(5, false).await.expect("cleared while on");
            assert_eq!(w.jobs_of(JobKind::VerifiedSync), 0, "the old sync is gone");
            assert_eq!(
                w.intents(),
                [(
                    format!("contact:{}", sid(5)),
                    intents::IDENTITY.to_owned(),
                    Some(0),
                    false
                )]
            );
        })
        .await;
}

// ---------------------------------------------------------------------------------------------
// The identity intent takes part in "the latest click wins by time" (A10c fix round, V1)
// ---------------------------------------------------------------------------------------------

impl World {
    /// A phone's `SyncMessage.verified` for person `n` and Bob's key, sent at `timestamp` and
    /// received at 1 (which is before every timestamp a test uses: use `phone_sync_received_at`
    /// when the receipt must be LATER than the click, as it is in life).
    fn phone_sync(&self, n: u128, state: i64, timestamp: u64) -> crate::safety::Remote {
        self.phone_sync_received_at(n, state, timestamp, 1)
    }

    /// The same, received at `now` (the receive zone's clock).
    fn phone_sync_received_at(
        &self,
        n: u128,
        state: i64,
        timestamp: u64,
        now: i64,
    ) -> crate::safety::Remote {
        let user = aci_user_id(aci(n)).unwrap();
        self.fixture
            .db
            .zone(|tx| {
                crate::safety::apply_remote_state(
                    tx,
                    &user,
                    &sid(n),
                    &bob_key(),
                    state,
                    now,
                    crate::safety::RemoteSource::Sync { timestamp },
                )
            })
            .unwrap()
    }

    /// A write in flight that carries the live identity intent `click`.
    fn carry_in_flight(&self, click: &intents::Intent) {
        let row = crate::storage::write::InFlight {
            version: 9,
            new_keys: vec![vec![0xa1; 16]],
            planned: vec![intents::Planned {
                target: click.target.clone(),
                field: intents::IDENTITY.to_owned(),
                value: click.value,
                seq: click.seq,
                rides: false,
                overtaken: false,
                prior: click.base,
                state_at_landing: false,
            }],
            generation: 0,
            planned_at: click.created_at,
        };
        self.fixture
            .db
            .zone(|tx| crate::storage::write::write_in_flight(tx, &row))
            .unwrap();
    }

    /// The stored safety-number state of person `n` (`identities.verified`).
    fn state_of(&self, n: u128) -> i64 {
        self.fixture
            .db
            .read(|c| crate::store::load_identity(c, &sid(n)))
            .unwrap()
            .expect("a stored key")
            .1
    }

    /// The live identity intent of person `n`.
    fn identity_intent(&self, n: u128) -> Option<intents::Intent> {
        self.fixture
            .db
            .read(intents::all_intents)
            .unwrap()
            .into_iter()
            .find(|i| i.target == format!("contact:{}", sid(n)) && i.field == intents::IDENTITY)
    }
}

/// A Mark with the switch on is an intent and no job, so the click's time is the intent's
/// `created_at`. A Verified sync from the phone that is NOT NEWER (older, or the same millisecond:
/// a tie keeps ours) is `Kept`: the state here is the click's, the intent stays and is written.
/// (Before V1 `own_sync_set_at` found no job, every phone sync was applied, and the write then put
/// the click's older state over the phone's, or the phone's older state over the click.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_older_verified_sync_is_kept_while_the_identity_intent_is_live() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut w = world();
            w.knows_the_key_of(5);
            w.phone_lists(5, |c| c.identity_key = bob_key().serialize().to_vec());
            w.switch_is(true);
            w.mark(5, true).await.expect("marked");
            let intent = w.identity_intent(5).expect("filed");
            let at = u64::try_from(intent.created_at).unwrap();

            for timestamp in [at - 1_000, at - 1, at] {
                assert!(
                    matches!(w.phone_sync(5, 0, timestamp), crate::safety::Remote::Kept),
                    "a sync at {timestamp} (the click at {at}) is not newer"
                );
                assert_eq!(w.state_of(5), 1, "the click's state stays");
                assert_eq!(w.identity_intent(5), Some(intent.clone()), "and is written");
            }
        })
        .await;
}

/// The other half: a Verified sync NEWER than the click is applied and the click is overruled:
/// the intent is deleted, so the next write does not carry an `identityState` over the phone's
/// newer state (it plans nothing at all here), and the core is told the phone's state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_newer_verified_sync_is_applied_and_retires_the_identity_intent() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut w = world();
            w.knows_the_key_of(5);
            w.phone_lists(5, |c| c.identity_key = bob_key().serialize().to_vec());
            w.switch_is(true);
            w.mark(5, true).await.expect("marked");
            let at = u64::try_from(w.identity_intent(5).unwrap().created_at).unwrap();

            let remote = w.phone_sync(5, 0, at + 1);
            let crate::safety::Remote::Applied { trust, .. } = remote else {
                panic!("a newer sync is applied: {remote:?}");
            };
            assert_eq!(trust, kourier_model::TrustLevel::Unverified);
            assert_eq!(w.state_of(5), 0, "the phone's later click stands here");
            assert!(w.identity_intent(5).is_none(), "the older click is gone");
            assert!(w.intents().is_empty(), "nothing is left to write");
        })
        .await;
}

/// A newer Verified sync meets a write IN FLIGHT that carries the click's state: that write still
/// puts the older state on the phone, so the phone's newer state is filed as a new intent after
/// it (X3's shape: its `base` is the value the write sets) and the next write puts it back. A
/// sync that states what the write carries needs nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_newer_verified_sync_during_a_write_that_carries_the_click_is_filed_after_it() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for (state, filed) in [(0, true), (1, false)] {
                let mut w = world();
                w.knows_the_key_of(5);
                w.phone_lists(5, |c| c.identity_key = bob_key().serialize().to_vec());
                w.switch_is(true);
                w.mark(5, true).await.expect("marked");
                let click = w.identity_intent(5).unwrap();
                w.carry_in_flight(&click);

                let at = u64::try_from(click.created_at).unwrap();
                assert!(matches!(
                    w.phone_sync(5, state, at + 1),
                    crate::safety::Remote::Applied { .. }
                ));
                if filed {
                    let next = w
                        .identity_intent(5)
                        .expect("the phone's state, filed after");
                    assert_eq!((next.value, next.base), (Some(state), click.value));
                    assert!(next.seq > click.seq, "after the carried click");
                    assert_eq!(next.key.as_deref(), Some(&bob_key().serialize()[..]));
                } else {
                    assert!(
                        w.identity_intent(5).is_none(),
                        "the write already carries that state"
                    );
                }
            }
        })
        .await;
}

/// Re-check N1: the intent refiled after a carried write is the PHONE's click, so it takes the
/// sync's own timestamp as its time, not the time the sync was received. A write in flight carries
/// his Verify; the phone's Clear (t1) and then its Verify (t2 > t1) arrive together, long after
/// both (`now` is past both timestamps: a catch-up after a sleep). The Clear is applied and filed
/// after the carried write at t1; the Verify is NEWER than that Clear, so it is applied too, and
/// the write in flight already carries its state: nothing is left to refile. (Stamped with the
/// receipt time the Verify would be `Kept` as "not newer" than the Clear and the phone's older
/// state would be written over its newest one.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_intent_refiled_after_a_carried_write_has_the_time_of_the_phones_click() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut w = world();
            w.knows_the_key_of(5);
            w.phone_lists(5, |c| c.identity_key = bob_key().serialize().to_vec());
            w.switch_is(true);
            w.mark(5, true).await.expect("marked");
            let click = w.identity_intent(5).unwrap();
            w.carry_in_flight(&click);

            let at = u64::try_from(click.created_at).unwrap();
            let (cleared_at, verified_at) = (at + 10, at + 20);
            let received = click.created_at + 60_000;

            assert!(matches!(
                w.phone_sync_received_at(5, 0, cleared_at, received),
                crate::safety::Remote::Applied { .. }
            ));
            let refiled = w
                .identity_intent(5)
                .expect("the phone's Clear, filed after the carried Verify");
            assert_eq!((refiled.value, refiled.base), (Some(0), click.value));
            assert_eq!(
                refiled.created_at,
                i64::try_from(cleared_at).unwrap(),
                "the time of the phone's click, not of the receipt"
            );
            assert_eq!(w.state_of(5), 0);

            let verified = w.phone_sync_received_at(5, 1, verified_at, received);
            assert!(
                matches!(verified, crate::safety::Remote::Applied { .. }),
                "the phone's Verify is newer than its Clear: {verified:?}"
            );
            assert_eq!(w.state_of(5), 1, "the newest click of the phone stands");
            assert!(
                w.identity_intent(5).is_none(),
                "the write in flight already carries VERIFIED"
            );
        })
        .await;
}

// ---------------------------------------------------------------------------------------------
// A dropped identity intent becomes the Verified sync (A10c fix round, V2)
// ---------------------------------------------------------------------------------------------

/// A Mark with the switch on and the phone's record holding the same key is an intent and NO
/// Verified sync. If the intent is dropped before the write goes out (here: he turns the switch
/// off), neither route carries the click: the other devices keep the old state with nothing marking
/// that anything is pending. The dropped intent therefore becomes the person's `VerifiedSync`,
/// with the click's key, state and time (the runner sends it only while key and state still equal
/// the stored row).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_identity_intent_becomes_the_verified_sync() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut w = world();
            w.knows_the_key_of(5);
            w.phone_lists(5, |c| c.identity_key = bob_key().serialize().to_vec());
            w.switch_is(true);
            w.mark(5, true).await.expect("marked");
            assert_eq!(w.jobs_of(JobKind::VerifiedSync), 0, "the intent alone");
            let click = w.identity_intent(5).expect("filed");

            w.set_switch(false).await.expect("off");
            assert!(w.identity_intent(5).is_none(), "dropped");
            assert_eq!(w.jobs_of(JobKind::VerifiedSync), 1, "and told by sync");
            let payload = w.payload_of(JobKind::VerifiedSync, &sid(5));
            assert_eq!(payload["state"], 1, "the click's state");
            assert!(
                payload["key"] == serde_json::json!(bob_key().serialize().to_vec()),
                "the click's key"
            );
            assert_eq!(payload["set_at"], click.created_at, "the click's time");
        })
        .await;
}
