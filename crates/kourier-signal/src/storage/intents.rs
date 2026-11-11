//! The intents of the Storage Service write engine (A10b, offline; `target/next/a10-refresh.md`
//! §3.2, §3.3, §3.8): what the user changed HERE (archive, pin, mute, marked unread, a safety
//! number's state) that the phone has not been told yet.
//!
//! A change reaches this module as one backend.db zone ([`intend`], [`intend_automatic`] for the
//! core's own clearing of a "marked unread" mark, [`intend_identity`] for Mark / Clear / Confirm
//! of a safety number): the GATE (the account's switch `account.roaming_writes`, a STORED record
//! of the chat's target, the pin limit counted over the stored record's entries), one row per
//! `(target, field)` in `storage_intents` (`value`, `base`, `shown`, `seq` from
//! `account.storage_intent_seq`), the couplings (archiving a pinned chat unpins it, pinning an
//! archived chat unarchives it), the mirrors (`peers.roaming_*` of a 1:1 chat and Note to Self,
//! the `groups` columns: what the core was last told), the `ConversationUpsert` (and the whole
//! pin list for a pin) into `core_outbox`, and the `storage_write` job a second later. The core
//! changes its row only from that event. Nothing is written to the Storage Service here. The
//! actor's arms call [`intend`] (`Command::ChangeConversation` of the four roaming changes),
//! [`file_durable`] (`Command::ChangeConversationDurable`) and, through `safety::set_verified`,
//! [`intend_identity`]; `Command::SetRoamingWrites` writes the switch and calls [`drop_all`]
//! (A10c C1). With no intent stored every merge and every read of the engine is a no-op.
//!
//! The rules in short (each has a test):
//!
//! - With the switch off a click is refused and NOTHING is stored; the automatic clear is
//!   acknowledged and dropped (it must not sit in `pending_commands` for ever).
//! - A click back to `shown` (what the core showed before the first click of the field) deletes
//!   the intent: a net-zero pair writes nothing (L6). Only while no in-flight write carries that
//!   `(target, field)`: once carried, a click back is a NEW intent whose `base` is the value the
//!   write in flight sets (X3).
//! - `seq` comes from a counter that only goes up (Y5), and an intent is replaced only by a
//!   newer `seq`: the intents are filed in the order the commands were received, so a mark made
//!   right after the automatic clear wins (A10a review F3).
//! - The automatic clear is filed only when the newest stored record, or a live intent, says
//!   marked (N-D), and not when that record was merged after the reading's `asked_at` (N-C): the
//!   phone's newer mark stays and the core is told so. N-C is skipped while a click of his on
//!   the mark is live (review L2: its value rules) and for a record OUR write put there
//!   (`storage_records.ours`, review N6). `asked_at` is the core's clock and
//!   `storage_records.updated_at` is the backend's: ONE clock in production (the process's
//!   `SystemClock`, handed to the core and, by the supervisor, to the backend as
//!   `AccountContext::clock`).
//! - Turning the switch off ([`drop_all`]) and age ([`drop_stale`]) delete the intents that no
//!   write in flight carries and restate the phone's value of what they dropped: each intent's
//!   `base` (the stored record's value when it was made, for `archived` the told base, review
//!   H2). A write that did not land puts back the bases it lent and, with the switch off,
//!   restates the phone's values from before it ([`Planned::prior`], review N1).
//! - A record the phone wrote is merged with the intents of its target in mind (§3.3, [`three_way`]
//!   and the `settle_*` functions at the end of this file): where the phone did not touch the field
//!   the intent STAYS and the merge states nothing, where it did the same the intent is deleted,
//!   where it did something else the PHONE WINS (the intent is deleted and the phone's value is
//!   stated). With no intent stored the merges do what they did before there were intents.
//!
//! The words are Storage Service words on purpose: this module is in `kourier-signal`.

use std::fmt;
use std::rc::Rc;

use kourier_backend_api::BackendEvent;
use kourier_db::Tx;
use kourier_db::rusqlite::{Connection, OptionalExtension as _, Row};
use kourier_model::{
    BackendError, CmdId, ConvKey, ConversationChange, ConversationPatch, ErrorKind, Patch,
};
use libsignal_core::{Aci, ServiceId};
use prost::Message as _;
use serde::Deserialize;
use tokio::sync::Notify;

use super::write::{self, InFlight, Landed};
use super::{
    ACCOUNT_TARGET, contact_sid, contact_target, fill_targets, group_target, muted_until, wire,
};
use crate::db::Account;
use crate::error::Result;
use crate::jobs::{self, JobKind};
use crate::outbox::append_events;
use crate::peers::{self, Roaming};
use crate::proto::{self, account_record::PinnedConversation, storage_record::Record};
use crate::service_id::{service_id_from_wire, service_id_user_id};
use crate::{features, groups};

/// `storage_intents.field` of the archive flag.
pub(crate) const ARCHIVED: &str = "archived";
/// `storage_intents.field` of the "marked unread" flag.
pub(crate) const MARKED_UNREAD: &str = "marked_unread";
/// `storage_intents.field` of the mute (`value` is the end in ms, `i64::MAX` = always, NULL =
/// unmuted).
pub(crate) const MUTED_UNTIL: &str = "muted_until";
/// `storage_intents.field` of a safety number's state (P5a; `key` holds the identity key).
pub(crate) const IDENTITY: &str = "identity";
/// `storage_intents.field` of a pin starts with this, followed by the pinned chat's target.
pub(super) const PIN_PREFIX: &str = "pin:";

/// The trailing debounce of the `storage_write` job: it runs this long after the last change.
const DEBOUNCE_MS: i64 = 1_000;
/// An intent older than this is dropped with a restatement (Y3 (3), the lead's number).
pub(crate) const STALE_AFTER_MS: i64 = 30 * 24 * 3600 * 1000;
/// The pin limit when the remote config states none (`global.pinnedChatLimit`, Desktop's
/// `getPinnedConversationLimit`).
const DEFAULT_PIN_LIMIT: u32 = 4;
/// The remote config entry of the pin limit.
pub(crate) const PIN_LIMIT_CONFIG: &str = "global.pinnedChatLimit";

/// `storage_intents.field` of the pin of the chat whose pin-list name is `chat_target`.
pub(crate) fn pin_field(chat_target: &str) -> String {
    format!("{PIN_PREFIX}{chat_target}")
}

/// The pin limit from the remote config's `global.pinnedChatLimit` (`None` or a value that is no
/// number: 4), the one function the caps and the gate both use.
pub(crate) fn pin_limit_of(value: Option<&str>) -> u32 {
    value
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_PIN_LIMIT)
}

/// The record target a 1:1 chat's flags live in: `account` for Note to Self (the account
/// record), `contact:<sid>` for anybody else.
pub(crate) fn direct_target(account: &Account, sid: &str) -> String {
    if ServiceId::from(account.aci).service_id_string() == sid {
        ACCOUNT_TARGET.to_owned()
    } else {
        contact_target(sid)
    }
}

// ---------------------------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------------------------

/// One row of `storage_intents`.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Intent {
    pub(crate) target: String,
    pub(crate) field: String,
    /// The wanted value: 0/1, the mute end (ms; NULL = unmuted), an identity state.
    pub(crate) value: Option<i64>,
    /// The phone's value when the intent was made: the newest stored record's (for `archived` the
    /// base the core was told, which A1 and X1 move: review H2), the value a write in flight sets
    /// when it carries the field, moved to the written value after a landing and back to the
    /// phone's value before the write when that write did not land ([`Planned::prior`], N1).
    pub(crate) base: Option<i64>,
    /// What the core showed before the first click of the field.
    pub(crate) shown: Option<i64>,
    /// An identity intent's key.
    pub(crate) key: Option<Vec<u8>>,
    pub(crate) seq: i64,
    /// The core's own clearing of a mark, not a click.
    pub(crate) automatic: bool,
    pub(crate) created_at: i64,
}

impl fmt::Debug for Intent {
    /// Log-safe: no target (it names a chat), no key, no value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = self.field.split(':').next().unwrap_or("");
        f.debug_struct("Intent")
            .field("field", &kind)
            .field("seq", &self.seq)
            .field("automatic", &self.automatic)
            .finish_non_exhaustive()
    }
}

const INTENT_COLUMNS: &str = "target, field, value, base, shown, key, seq, automatic, created_at";

fn intent_of(row: &Row<'_>) -> kourier_db::rusqlite::Result<Intent> {
    Ok(Intent {
        target: row.get(0)?,
        field: row.get(1)?,
        value: row.get(2)?,
        base: row.get(3)?,
        shown: row.get(4)?,
        key: row.get(5)?,
        seq: row.get(6)?,
        automatic: row.get(7)?,
        created_at: row.get(8)?,
    })
}

fn load_intent(conn: &Connection, target: &str, field: &str) -> Result<Option<Intent>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {INTENT_COLUMNS} FROM storage_intents WHERE target = ?1 AND field = ?2"
        ))?
        .query_row((target, field), intent_of)
        .optional()?)
}

/// Every live intent, oldest `seq` first.
pub(crate) fn all_intents(conn: &Connection) -> Result<Vec<Intent>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {INTENT_COLUMNS} FROM storage_intents ORDER BY seq"
    ))?;
    let rows = stmt
        .query_map([], intent_of)?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    Ok(rows)
}

/// The live pin intents, oldest first.
fn pin_intents(conn: &Connection) -> Result<Vec<Intent>> {
    Ok(all_intents(conn)?
        .into_iter()
        .filter(|i| i.target == ACCOUNT_TARGET && i.field.starts_with(PIN_PREFIX))
        .collect())
}

fn delete_intent(conn: &Connection, target: &str, field: &str) -> Result<()> {
    conn.prepare_cached("DELETE FROM storage_intents WHERE target = ?1 AND field = ?2")?
        .execute((target, field))?;
    Ok(())
}

/// The next `seq`: a counter in `account` that only goes up, so a seq is never reused after
/// intents were deleted (critic Y5).
fn next_seq(conn: &Connection) -> Result<i64> {
    Ok(conn
        .prepare_cached(
            "UPDATE account SET storage_intent_seq = storage_intent_seq + 1 WHERE id = 1
             RETURNING storage_intent_seq",
        )?
        .query_row([], |r| r.get(0))?)
}

/// Stores `intent`, replacing the row of its `(target, field)` only by a NEWER `seq`: the intents
/// are filed in the order the commands were received.
fn put_intent(conn: &Connection, intent: &Intent) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO storage_intents(target, field, value, base, shown, key, seq, automatic,
           created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(target, field) DO UPDATE SET value = excluded.value, base = excluded.base,
           shown = excluded.shown, key = excluded.key, seq = excluded.seq,
           automatic = excluded.automatic, created_at = excluded.created_at
         WHERE excluded.seq > storage_intents.seq",
    )?
    .execute((
        &intent.target,
        &intent.field,
        intent.value,
        intent.base,
        intent.shown,
        &intent.key,
        intent.seq,
        intent.automatic,
        intent.created_at,
    ))?;
    Ok(())
}

/// What a write carries for one `(target, field)`: the value it sets and the `seq` of the intent it
/// closes. The plan (`write::plan`) produces them and the in-flight row stores the list as JSON
/// (`storage_write_inflight.planned`, the shape is written in migration 0025); this module reads it
/// back, so the two sides share this one type.
#[derive(Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
pub(crate) struct Planned {
    pub(crate) target: String,
    pub(crate) field: String,
    /// The written value: 0/1, the mute's end in ms, an identity state; `null` = unmuted.
    #[serde(default)]
    pub(crate) value: Option<i64>,
    pub(crate) seq: i64,
    /// X1's ride-along (review H1): the `archived` the core was last told, written into a record
    /// rotated for another field. No intent asked for it (`seq` 0), so the landing closes none for
    /// it; it is carried all the same: a click takes it as its `base`, the landing moves a live
    /// archive intent's base to it, `drop_all` treats the field as carried and a message that
    /// brings the chat back after it was written as `1` files `archived = false`. Written into the
    /// JSON only when true (`"rides": true`); a row written before it existed reads as false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) rides: bool,
    /// A message brought the chat back after this `archived` entry was planned, whatever it sets
    /// (A1's `chat_left_the_archive`, reviews L1 and R2-2 (1)): a landing with the switch OFF does
    /// not state it, and a write that does not land does not restate its `prior` (with
    /// the switch on the core is told nothing at a landing anyway). Written into the JSON only when
    /// true (`"overtaken": true`); a row written before it existed reads as false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) overtaken: bool,
    /// The phone's value of the field BEFORE the write (review N1 (b)): the carried intent's
    /// `base` when the plan was made (for `archived` the told base, review H2), a ride-along's own
    /// value. Every exit of a write that did NOT land ([`not_landed`]) puts it back as the `base`
    /// AND the `shown` of a click filed while the row existed (X3: it took the written value for
    /// both, review R2-1), and nothing was written.
    /// Written into the JSON only when set (`"prior": ..`); `null`/absent = unmuted or not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) prior: Option<i64>,
    /// D2: a NEWER click on this carried field was deleted without a restatement, because the
    /// landing was to state the written value ([`drop_where`]: the switch went off while this write
    /// was in flight). That promise is kept here, in the row, whatever the switch says when the
    /// answer comes (it may have been turned on again): [`land`] states the written value of a
    /// marked entry no live intent holds, and [`not_landed`] its `prior`. Written into the JSON
    /// only when true (`"state_at_landing": true`); a row written before it existed reads as false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) state_at_landing: bool,
}

/// `skip_serializing_if` of [`Planned::rides`] (serde hands a reference).
fn is_false(value: &bool) -> bool {
    !*value
}

impl fmt::Debug for Planned {
    /// Log-safe: no target (it names a chat), no value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = self.field.split(':').next().unwrap_or("");
        f.debug_struct("Planned")
            .field("field", &kind)
            .field("seq", &self.seq)
            .field("rides", &self.rides)
            .field("overtaken", &self.overtaken)
            .field("state_at_landing", &self.state_at_landing)
            .finish_non_exhaustive()
    }
}

/// The `planned` list of the in-flight row (empty when there is none, or when this build cannot
/// read the row: [`write::find_in_flight`] deletes and logs it, review L3).
pub(super) fn planned(conn: &Connection) -> Result<Vec<Planned>> {
    Ok(write::read_in_flight(conn)?.map_or_else(Vec::new, |row| row.planned))
}

fn carried_by_inflight(conn: &Connection, target: &str, field: &str) -> Result<Option<Planned>> {
    Ok(planned(conn)?
        .into_iter()
        .find(|p| p.target == target && p.field == field))
}

// ---------------------------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------------------------

/// Why a click was refused. Nothing is stored when it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The account's switch "Sync chat changes with my phone" is off.
    SwitchOff,
    /// Not one of the four roaming settings.
    NotRoaming,
    /// The phone has no record of this chat (a message request, a person known only by a number,
    /// a group it does not list), or none the pin list could name.
    NotListed,
    /// The phone's record of this chat does not decode.
    Unreadable,
    /// Note to Self has no mute.
    NotMutable,
    /// The account has pinned as many chats as it may.
    TooManyPins,
}

impl Refused {
    /// The contract form: fixed, log-safe words (they name no chat).
    pub(crate) fn to_backend_error(self) -> BackendError {
        match self {
            Self::SwitchOff => {
                BackendError::unsupported("chat changes are not synced for this account")
            }
            Self::NotRoaming => BackendError::unsupported("not a setting the phone keeps"),
            Self::NotListed => BackendError::new(
                ErrorKind::InvalidInput,
                "the phone does not list this chat yet",
            ),
            Self::Unreadable => BackendError::new(
                ErrorKind::InvalidInput,
                "the phone's record of this chat cannot be read",
            ),
            Self::NotMutable => {
                BackendError::new(ErrorKind::InvalidInput, "this chat cannot be muted")
            }
            // `TooLarge` (A10d L6, architecture §4.2): on `ChangeConversation` it means the pin
            // limit, so the window names the account's limit as it does for the core's own
            // `Refusal::TooMany` (a pin the core could not count: the phone lists a chat the core
            // does not), never "the network failed".
            Self::TooManyPins => BackendError::new(
                ErrorKind::TooLarge,
                "the account has pinned as many chats as it may",
            ),
        }
    }
}

/// Whether the account's switch is on (`account.roaming_writes`). Read in the zone of every
/// change; A10c's job reads it again before its PUT.
pub(crate) fn switch_is_on(conn: &Connection) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT roaming_writes FROM account WHERE id = 1")?
        .query_row([], |r| r.get::<_, bool>(0))
        .optional()?
        .unwrap_or(false))
}

/// How a chat is reached in the stored records.
#[derive(Clone, PartialEq, Eq)]
enum Kind {
    Contact { sid: String },
    NoteToSelf { sid: String },
    Group { id: Vec<u8> },
}

/// A chat the core named, resolved to what the engine edits.
struct Chat {
    conv: ConvKey,
    kind: Kind,
    /// The target of the record the chat's flags live in.
    target: String,
    /// What the pin list calls the chat.
    pin_target: String,
    /// The ACI a new pin entry names (`None` for a person known only by a PNI: not pinnable).
    pin_aci: Option<Aci>,
}

fn chat_of(
    conn: &Connection,
    account: &Account,
    conv: &ConvKey,
) -> Result<std::result::Result<Chat, Refused>> {
    match conv {
        ConvKey::Direct(user) => {
            let Some(service_id) = features::direct_service_id(account, user) else {
                return Ok(Err(Refused::NotListed));
            };
            let sid = service_id.service_id_string();
            let own = service_id == ServiceId::from(account.aci);
            let pin_aci = match service_id {
                ServiceId::Aci(aci) => Some(aci),
                ServiceId::Pni(_) => None,
            };
            Ok(Ok(Chat {
                conv: conv.clone(),
                target: direct_target(account, &sid),
                pin_target: contact_target(&sid),
                pin_aci,
                kind: if own {
                    Kind::NoteToSelf { sid }
                } else {
                    Kind::Contact { sid }
                },
            }))
        }
        ConvKey::Group(_) => {
            let Some(id) = groups::id_of_conv(conv) else {
                return Ok(Err(Refused::NotListed));
            };
            let Some(master) = groups::master_of(conn, &id)? else {
                return Ok(Err(Refused::NotListed));
            };
            let target = group_target(&master);
            Ok(Ok(Chat {
                conv: conv.clone(),
                pin_target: target.clone(),
                target,
                pin_aci: None,
                kind: Kind::Group { id },
            }))
        }
        ConvKey::Broadcast(_) | ConvKey::Channel(_) => Ok(Err(Refused::NotListed)),
    }
}

/// The chat a record target belongs to (for the restatement of dropped intents); `None` for a
/// target this build cannot resolve.
fn chat_of_target(account: &Account, target: &str) -> Option<Chat> {
    if target == ACCOUNT_TARGET {
        let sid = ServiceId::from(account.aci).service_id_string();
        let user = service_id_user_id(ServiceId::from(account.aci)).ok()?;
        return Some(Chat {
            conv: ConvKey::Direct(user),
            kind: Kind::NoteToSelf { sid: sid.clone() },
            target: ACCOUNT_TARGET.to_owned(),
            pin_target: contact_target(&sid),
            pin_aci: Some(account.aci),
        });
    }
    if let Some(sid) = target.strip_prefix("contact:") {
        let service_id = ServiceId::parse_from_service_id_string(sid)?;
        return Some(Chat {
            conv: ConvKey::Direct(service_id_user_id(service_id).ok()?),
            kind: Kind::Contact {
                sid: sid.to_owned(),
            },
            target: target.to_owned(),
            pin_target: target.to_owned(),
            pin_aci: match service_id {
                ServiceId::Aci(aci) => Some(aci),
                ServiceId::Pni(_) => None,
            },
        });
    }
    if let Some(hex) = target.strip_prefix("group:") {
        let master = groups::parse_job_key(hex)?;
        let keys = groups::GroupKeys::from_slice(&master)?;
        return Some(Chat {
            conv: keys.conv_key(),
            kind: Kind::Group {
                id: keys.id.to_vec(),
            },
            target: target.to_owned(),
            pin_target: target.to_owned(),
            pin_aci: None,
        });
    }
    None
}

/// The newest stored record of a target.
#[allow(
    clippy::large_enum_variant,
    reason = "a value of a few lines of a zone, never kept or collected"
)]
enum Stored {
    Missing,
    Unreadable,
    Found {
        record: proto::StorageRecord,
        /// When it was merged (the backend's clock).
        updated_at: i64,
        /// A record OUR write put there (`storage_records.ours`, review N6), not the phone's.
        ours: bool,
    },
}

fn newest_record(conn: &Connection, target: &str) -> Result<Stored> {
    let row: Option<(Vec<u8>, i64, bool)> = conn
        .prepare_cached(
            "SELECT record, updated_at, ours FROM storage_records WHERE target = ?1
             ORDER BY updated_at DESC, rowid DESC LIMIT 1",
        )?
        .query_row([target], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    Ok(match row {
        None => Stored::Missing,
        Some((raw, updated_at, ours)) => match proto::StorageRecord::decode(&raw[..]) {
            Ok(record) => Stored::Found {
                record,
                updated_at,
                ours,
            },
            Err(_) => Stored::Unreadable,
        },
    })
}

/// The three flags a record states for its chat.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Flags {
    pub(crate) archived: bool,
    pub(crate) marked_unread: bool,
    /// The mute's end in ms (`i64::MAX` = for ever), `None` = not muted.
    pub(crate) muted_until: Option<i64>,
}

fn flags_of(chat: &Chat, record: &proto::StorageRecord) -> Option<Flags> {
    match (&chat.kind, &record.record) {
        (Kind::Contact { .. }, Some(Record::Contact(c))) => Some(Flags {
            archived: c.archived,
            marked_unread: c.marked_unread,
            muted_until: muted_until(c.muted_until_timestamp),
        }),
        (Kind::Group { .. }, Some(Record::GroupV2(g))) => Some(Flags {
            archived: g.archived,
            marked_unread: g.marked_unread,
            muted_until: muted_until(g.muted_until_timestamp),
        }),
        (Kind::NoteToSelf { .. }, Some(Record::Account(a))) => Some(Flags {
            archived: a.note_to_self_archived,
            marked_unread: a.note_to_self_marked_unread,
            muted_until: None,
        }),
        _ => None,
    }
}

/// The chat and the flags of its stored record, or why not.
fn writable(
    conn: &Connection,
    account: &Account,
    conv: &ConvKey,
) -> Result<std::result::Result<(Chat, Flags), Refused>> {
    let chat = match chat_of(conn, account, conv)? {
        Ok(chat) => chat,
        Err(refused) => return Ok(Err(refused)),
    };
    let flags = match newest_record(conn, &chat.target)? {
        Stored::Missing => return Ok(Err(Refused::NotListed)),
        Stored::Unreadable => return Ok(Err(Refused::Unreadable)),
        Stored::Found { record, .. } => match flags_of(&chat, &record) {
            Some(flags) => flags,
            None => return Ok(Err(Refused::Unreadable)),
        },
    };
    Ok(Ok((chat, flags)))
}

// ---------------------------------------------------------------------------------------------
// Mirrors
// ---------------------------------------------------------------------------------------------

/// What the core was last told of the chat's archive state and mute.
fn mirror_of(conn: &Connection, account: &Account, chat: &Chat, now: i64) -> Result<Roaming> {
    match &chat.kind {
        Kind::Contact { sid } | Kind::NoteToSelf { sid } => {
            match peers::load(conn, sid)?.roaming {
                Some(told) => Ok(told),
                // A profile from before the base: learned once from the newest record (A1).
                None => Ok(super::fill_roaming(conn, account, sid, now)?.0),
            }
        }
        Kind::Group { id } => {
            let (archived, muted_until) = groups::archive_state(conn, id)?.unwrap_or((false, None));
            Ok(Roaming {
                archived,
                muted_until,
            })
        }
    }
}

/// What the core was last told of each target's archive state, for the plan's rule X1 (a rotated
/// record writes the told base, not the record's own flag): `contact:<sid>` and `account` (Note to
/// Self) from the peer's base (learned once from the newest record when a profile from before
/// 0023 has none, as [`mirror_of`] does), `group:<hex>` from the `groups` column. A target with no
/// base is left out: the plan then keeps the record's flag as the phone wrote it. Call it in a
/// zone, right before planning (the merge of the fetched manifest moves the bases first).
pub(super) fn told_archived(
    conn: &Connection,
    account: &Account,
    targets: &[String],
    now: i64,
) -> Result<std::collections::HashMap<String, bool>> {
    let mut told = std::collections::HashMap::new();
    for target in targets {
        let Some(chat) = chat_of_target(account, target) else {
            continue;
        };
        let archived = match &chat.kind {
            Kind::Group { id } => match groups::archive_state(conn, id)? {
                Some((archived, _)) => archived,
                None => continue,
            },
            Kind::Contact { .. } | Kind::NoteToSelf { .. } => {
                mirror_of(conn, account, &chat, now)?.archived
            }
        };
        told.insert(target.clone(), archived);
    }
    Ok(told)
}

/// Writes the mirror: the 1:1 chat's `peers.roaming_*` or the group's columns. `marked_unread`
/// is a group column only (a 1:1 chat's mark has no mirror: the merge states the record's).
fn set_mirror(
    conn: &Connection,
    chat: &Chat,
    roaming: Roaming,
    marked_unread: Option<bool>,
    now: i64,
) -> Result<()> {
    match &chat.kind {
        Kind::Contact { sid } | Kind::NoteToSelf { sid } => {
            peers::set_roaming(conn, sid, roaming, now)
        }
        Kind::Group { id } => {
            let Some(row) = groups::load_by_id(conn, id)? else {
                return Ok(());
            };
            groups::set_roaming(
                conn,
                id,
                roaming.archived,
                marked_unread.unwrap_or(row.marked_unread),
                roaming.muted_until,
                now,
            )
        }
    }
}

/// What the core shows of the mark: a group's mirror, else the stored record's.
fn shown_mark(conn: &Connection, chat: &Chat, flags: Flags) -> Result<bool> {
    match &chat.kind {
        Kind::Group { id } => {
            Ok(groups::load_by_id(conn, id)?.map_or(flags.marked_unread, |g| g.marked_unread))
        }
        _ => Ok(flags.marked_unread),
    }
}

// ---------------------------------------------------------------------------------------------
// The pin list
// ---------------------------------------------------------------------------------------------

/// The name of a pin-list entry in `storage_intents`' terms: `contact:<sid>` or `group:<hex>`;
/// `None` for an entry this build cannot name (release notes, a newer kind, a broken id).
pub(super) fn entry_target(entry: &PinnedConversation) -> Option<String> {
    use proto::account_record::pinned_conversation::Identifier as Pin;
    match &entry.identifier {
        Some(Pin::Contact(contact)) => {
            let service_id = service_id_from_wire(
                Some(contact.service_id_binary.as_slice()),
                Some(contact.service_id.as_str()),
            )
            .ok()
            .flatten()?;
            Some(contact_target(&service_id.service_id_string()))
        }
        Some(Pin::GroupMasterKey(master)) if master.len() == 32 => Some(group_target(master)),
        _ => None,
    }
}

/// The bytes of a new pin entry for the pin-list name `target`, in the form of his Android phone
/// (BQ5): an ACI contact or a group (`None` for a person known only by a PNI and a name this build
/// cannot read).
pub(super) fn new_entry_bytes(target: &str) -> Option<Vec<u8>> {
    Some(if let Some(sid) = target.strip_prefix("contact:") {
        match ServiceId::parse_from_service_id_string(sid)? {
            ServiceId::Aci(aci) => wire::contact_pin_entry(aci),
            ServiceId::Pni(_) => return None,
        }
    } else {
        let master = groups::parse_job_key(target.strip_prefix("group:")?)?;
        wire::group_pin_entry(&<[u8; 32]>::try_from(master).ok()?)
    })
}

/// A new pin entry for the pin-list name `target` (see [`new_entry_bytes`]).
fn new_entry(target: &str) -> Option<PinnedConversation> {
    PinnedConversation::decode(&new_entry_bytes(target)?[..]).ok()
}

/// The stored account record's pin list and the live pin intents.
struct Pins {
    record: proto::AccountRecord,
    intents: Vec<Intent>,
}

impl Pins {
    /// `None` when there is no readable stored account record (nothing can be pinned).
    fn load(conn: &Connection) -> Result<Option<Self>> {
        Ok(match newest_record(conn, ACCOUNT_TARGET)? {
            Stored::Found {
                record:
                    proto::StorageRecord {
                        record: Some(Record::Account(record)),
                    },
                ..
            } => Some(Self {
                record,
                intents: pin_intents(conn)?,
            }),
            _ => None,
        })
    }

    /// How many entries the account has pinned: the STORED record's entries, all of them (a group
    /// not fetched yet, a person known only by a number and an entry this build cannot read
    /// included: the phone counts them, and the written list keeps them), with the live pin
    /// intents applied (critic M3).
    fn count(&self) -> usize {
        let mut count = self.record.pinned_conversations.len();
        for intent in &self.intents {
            let target = intent.field.strip_prefix(PIN_PREFIX).unwrap_or("");
            let listed = self
                .record
                .pinned_conversations
                .iter()
                .filter(|e| entry_target(e).as_deref() == Some(target))
                .count();
            if intent.value == Some(1) {
                if listed == 0 {
                    count += 1;
                }
            } else {
                count = count.saturating_sub(listed);
            }
        }
        count
    }

    /// Whether the chat is pinned: the live intent says, else the stored record.
    fn is_pinned(&self, pin_target: &str) -> bool {
        let field = pin_field(pin_target);
        match self.intents.iter().rev().find(|i| i.field == field) {
            Some(intent) => intent.value == Some(1),
            None => self
                .record
                .pinned_conversations
                .iter()
                .any(|e| entry_target(e).as_deref() == Some(pin_target)),
        }
    }

    /// The list the written record will hold: the stored entries without the removals, a new
    /// entry for each addition in `seq` order, every other entry as it is.
    fn effective_entries(&self) -> Vec<PinnedConversation> {
        let removed = |target: &str| {
            let field = pin_field(target);
            self.intents
                .iter()
                .any(|i| i.field == field && i.value != Some(1))
        };
        let mut entries: Vec<PinnedConversation> = self
            .record
            .pinned_conversations
            .iter()
            .filter(|e| entry_target(e).is_none_or(|t| !removed(&t)))
            .cloned()
            .collect();
        for intent in self.intents.iter().filter(|i| i.value == Some(1)) {
            let Some(target) = intent.field.strip_prefix(PIN_PREFIX) else {
                continue;
            };
            if entries
                .iter()
                .any(|e| entry_target(e).as_deref() == Some(target))
            {
                continue;
            }
            if let Some(entry) = new_entry(target) {
                entries.push(entry);
            }
        }
        entries
    }

    /// The list the core is told: what `pinned_chats` names of the effective entries.
    fn list_event(&self, conn: &Connection) -> Result<BackendEvent> {
        let mut record = self.record.clone();
        record.pinned_conversations = self.effective_entries();
        let pinned = super::pinned_chats(conn, &record)?;
        Ok(BackendEvent::PinnedConversations(
            pinned.into_iter().map(|p| p.conv).collect(),
        ))
    }
}

/// The pin limit of the account: the remote config's, else 4.
pub(super) fn pin_limit(conn: &Connection) -> Result<u32> {
    Ok(pin_limit_of(
        crate::maintenance::remote_config_value(conn, PIN_LIMIT_CONFIG)?.as_deref(),
    ))
}

// ---------------------------------------------------------------------------------------------
// Filing
// ---------------------------------------------------------------------------------------------

/// A change to file.
struct Fresh<'a> {
    target: &'a str,
    field: &'a str,
    value: Option<i64>,
    /// The phone's value: the newest stored record's, for `archived` the told base (review H2).
    base: Option<i64>,
    /// What the core shows now.
    shown: Option<i64>,
    key: Option<&'a [u8]>,
    automatic: bool,
}

/// What [`file`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filing {
    Filed,
    /// A click back to `shown`: the intent is gone.
    Undone,
}

/// Files one change on `(target, field)`, in the order received. `created_at` is the time of the
/// click (ms): the time of the command, except for the phone's click that [`retire_identity`]
/// refiles, which has its own.
fn file(conn: &Connection, fresh: &Fresh<'_>, created_at: i64) -> Result<Filing> {
    let live = load_intent(conn, fresh.target, fresh.field)?;
    let carried = carried_by_inflight(conn, fresh.target, fresh.field)?;
    // L6: a click back to what the core showed before the first click writes nothing, while no
    // write in flight carries the field (X3), and not over the core's own clearing.
    if let Some(live) = &live
        && carried.is_none()
        && !live.automatic
        && fresh.key.is_none()
        && fresh.value == live.shown
    {
        delete_intent(conn, fresh.target, fresh.field)?;
        return Ok(Filing::Undone);
    }
    let (base, shown) = match (&live, &carried) {
        // The write in flight sets `planned.value`: that is the phone's value once it lands.
        (_, Some(planned)) => (
            planned.value,
            live.as_ref().map_or(planned.value, |l| l.value),
        ),
        (Some(live), None) => (live.base, live.shown),
        (None, None) => (fresh.base, fresh.shown),
    };
    let seq = next_seq(conn)?;
    put_intent(
        conn,
        &Intent {
            target: fresh.target.to_owned(),
            field: fresh.field.to_owned(),
            value: fresh.value,
            base,
            shown,
            key: fresh.key.map(<[u8]>::to_vec),
            seq,
            automatic: fresh.automatic,
            created_at,
        },
    )?;
    Ok(Filing::Filed)
}

/// Makes the `storage_write` job due a second after the last change, when an intent is left (also
/// at every start, critic Y3: the job row of intents an earlier run left may have been dropped).
pub(crate) fn schedule_write(conn: &Connection, now: i64) -> Result<()> {
    let left: bool = conn
        .prepare_cached("SELECT EXISTS (SELECT 1 FROM storage_intents)")?
        .query_row([], |r| r.get(0))?;
    if left {
        jobs::delay_to(
            conn,
            JobKind::StorageWrite,
            "",
            now.saturating_add(DEBOUNCE_MS),
            now,
        )?;
    }
    Ok(())
}

fn flag_value(on: bool) -> Option<i64> {
    Some(i64::from(on))
}

/// A user's change of one of the four roaming settings of a chat, in the zone of the command
/// (`Command::ChangeConversation`): the gate, the intent, the couplings, the mirrors, the events
/// and the job. `Err(Refused)` stores and states nothing.
pub(crate) fn intend(
    tx: &mut Tx<'_>,
    account: &Account,
    change: &ConversationChange,
    conv: &ConvKey,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<std::result::Result<(), Refused>> {
    if !change.is_roaming() {
        return Ok(Err(Refused::NotRoaming));
    }
    if !switch_is_on(tx)? {
        return Ok(Err(Refused::SwitchOff));
    }
    fill_targets(tx, account)?;
    let (chat, flags) = match writable(tx, account, conv)? {
        Ok(found) => found,
        Err(refused) => return Ok(Err(refused)),
    };
    let mut patch = ConversationPatch::new(chat.conv.clone());
    let mut patched = false;
    let mut pins_changed = None;
    match change {
        ConversationChange::SetArchived { archived } => {
            let mut told = mirror_of(tx, account, &chat, now)?;
            file(
                tx,
                &Fresh {
                    target: &chat.target,
                    field: ARCHIVED,
                    value: flag_value(*archived),
                    // The told base, not the record's flag (review H2): A1 moves it when a message
                    // brings the chat back (the phone does the same locally), and X1 writes it.
                    base: flag_value(told.archived),
                    shown: flag_value(told.archived),
                    key: None,
                    automatic: false,
                },
                now,
            )?;
            told.archived = *archived;
            set_mirror(tx, &chat, told, None, now)?;
            patch.archived = Some(*archived);
            patched = true;
            // Archiving a pinned chat unpins it (Signal's rule).
            if *archived
                && let Some(pins) = Pins::load(tx)?
                && pins.is_pinned(&chat.pin_target)
            {
                file_pin(tx, &chat, false, now)?;
                pins_changed = Some(());
            }
        }
        ConversationChange::SetPinned { pinned } => {
            let Some(pins) = Pins::load(tx)? else {
                return Ok(Err(Refused::NotListed));
            };
            if *pinned && chat.pin_aci.is_none() && !matches!(chat.kind, Kind::Group { .. }) {
                // A person known by a PNI only has no pin entry this build writes.
                return Ok(Err(Refused::NotListed));
            }
            if *pinned && !pins.is_pinned(&chat.pin_target) {
                let limit = usize::try_from(pin_limit(tx)?).unwrap_or(usize::MAX);
                if pins.count() >= limit {
                    return Ok(Err(Refused::TooManyPins));
                }
            }
            file_pin(tx, &chat, *pinned, now)?;
            pins_changed = Some(());
            // Pinning an archived chat unarchives it (Signal's rule).
            if *pinned {
                let mut told = mirror_of(tx, account, &chat, now)?;
                if told.archived {
                    file(
                        tx,
                        &Fresh {
                            target: &chat.target,
                            field: ARCHIVED,
                            value: flag_value(false),
                            // The told base (review H2), as for a click on Archive.
                            base: flag_value(told.archived),
                            shown: flag_value(true),
                            key: None,
                            automatic: false,
                        },
                        now,
                    )?;
                    told.archived = false;
                    set_mirror(tx, &chat, told, None, now)?;
                    patch.archived = Some(false);
                    patched = true;
                }
            }
        }
        ConversationChange::SetMuted { until } => {
            if matches!(chat.kind, Kind::NoteToSelf { .. }) {
                return Ok(Err(Refused::NotMutable));
            }
            let mut told = mirror_of(tx, account, &chat, now)?;
            file(
                tx,
                &Fresh {
                    target: &chat.target,
                    field: MUTED_UNTIL,
                    value: *until,
                    base: flags.muted_until,
                    shown: told.muted_until,
                    key: None,
                    automatic: false,
                },
                now,
            )?;
            told.muted_until = *until;
            set_mirror(tx, &chat, told, None, now)?;
            patch.muted_until = until.map_or(Patch::Clear, Patch::Set);
            patched = true;
        }
        ConversationChange::SetMarkedUnread { marked_unread } => {
            let shown = shown_mark(tx, &chat, flags)?;
            file(
                tx,
                &Fresh {
                    target: &chat.target,
                    field: MARKED_UNREAD,
                    value: flag_value(*marked_unread),
                    base: flag_value(flags.marked_unread),
                    shown: flag_value(shown),
                    key: None,
                    automatic: false,
                },
                now,
            )?;
            let told = mirror_of(tx, account, &chat, now)?;
            set_mirror(tx, &chat, told, Some(*marked_unread), now)?;
            patch.marked_unread = Some(*marked_unread);
            patched = true;
        }
        // `is_roaming` was checked above; a variant it does not list is refused there.
        ConversationChange::SetBlocked { .. }
        | ConversationChange::SetTimer { .. }
        | ConversationChange::AcceptRequest
        | ConversationChange::Leave => return Ok(Err(Refused::NotRoaming)),
    }
    let mut events = Vec::with_capacity(2);
    if patched {
        events.push(BackendEvent::ConversationUpsert(patch));
    }
    if pins_changed.is_some()
        && let Some(pins) = Pins::load(tx)?
    {
        events.push(pins.list_event(tx)?);
    }
    append_events(tx, &events, now, wake)?;
    schedule_write(tx, now)?;
    Ok(Ok(()))
}

/// Files the pin (or unpin) of `chat` on the account record.
fn file_pin(conn: &Connection, chat: &Chat, pinned: bool, now: i64) -> Result<()> {
    let Some(pins) = Pins::load(conn)? else {
        return Ok(());
    };
    let listed = pins
        .record
        .pinned_conversations
        .iter()
        .any(|e| entry_target(e).as_deref() == Some(chat.pin_target.as_str()));
    let shown = pins.is_pinned(&chat.pin_target);
    file(
        conn,
        &Fresh {
            target: ACCOUNT_TARGET,
            field: &pin_field(&chat.pin_target),
            value: flag_value(pinned),
            base: flag_value(listed),
            shown: flag_value(shown),
            key: None,
            automatic: false,
        },
        now,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The automatic clear
// ---------------------------------------------------------------------------------------------

/// What became of the core's own clearing of a mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Automatic {
    /// Filed as an automatic intent (or it cancelled a click's mark that was not written yet).
    Filed,
    /// Nothing to write: the switch is off, the chat has no readable record, the phone has no
    /// mark (N-D), or a click of his that is not written yet rules (review L2).
    Dropped,
    /// The phone's record says marked and was merged after the reading (N-C): its mark stays, and
    /// the core was told so.
    PhoneKept,
}

/// The core's durable clearing of a mark (`Command::ChangeConversationDurable`), in its zone:
/// the same gate, filed with `automatic = 1` in the order received, and `CommandPersisted`
/// after it. Returns whether a job should be woken (an intent was filed).
#[allow(clippy::too_many_arguments, reason = "one zone's inputs, called once")]
pub(crate) fn file_durable(
    tx: &mut Tx<'_>,
    account: &Account,
    cmd: CmdId,
    conv: &ConvKey,
    change: &ConversationChange,
    asked_at: i64,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<bool> {
    let outcome = intend_automatic(tx, account, change, conv, asked_at, now, wake)?;
    crate::commands::confirm(tx, cmd, now, wake)?;
    Ok(outcome == Automatic::Filed)
}

/// The decision of [`file_durable`] without the acknowledgement.
pub(crate) fn intend_automatic(
    tx: &mut Tx<'_>,
    account: &Account,
    change: &ConversationChange,
    conv: &ConvKey,
    asked_at: i64,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<Automatic> {
    if !matches!(
        change,
        ConversationChange::SetMarkedUnread {
            marked_unread: false
        }
    ) {
        return Ok(Automatic::Dropped);
    }
    if !switch_is_on(tx)? {
        return Ok(Automatic::Dropped);
    }
    fill_targets(tx, account)?;
    let Ok(chat) = chat_of(tx, account, conv)? else {
        return Ok(Automatic::Dropped);
    };
    let Stored::Found {
        record,
        updated_at,
        ours,
    } = newest_record(tx, &chat.target)?
    else {
        return Ok(Automatic::Dropped);
    };
    let Some(flags) = flags_of(&chat, &record) else {
        return Ok(Automatic::Dropped);
    };
    // A click of his (Mark as Read / Mark as Unread) that is not written yet rules: its value is
    // what the chat is to show and what the phone is to get, so no statement of the phone's mark
    // is made over it (review L2: a record merged after the reading for an unrelated field has
    // `updated_at > asked_at` too, and its OLD mark equals the click's base).
    let live = load_intent(tx, &chat.target, MARKED_UNREAD)?;
    let click_rules = live.as_ref().is_some_and(|l| !l.automatic);
    // N-C: the phone's mark is newer than the reading (same clock, ms): it stays. Not a record of
    // OURS (`storage_records.ours`, review N6): our own landed write is stamped with the time of
    // its landing, and its mark is the click this reading clears, not a newer mark of the phone.
    if flags.marked_unread && updated_at > asked_at && !click_rules && !ours {
        // An OLDER automatic clear of this chat that is still queued would erase the phone's
        // newer mark: it goes (a click is the user's own word and stays). The merge could not
        // retire it: a record that still says marked is `theirs == base` to the three-way rule.
        let older_clear = live.as_ref().is_some_and(|i| i.automatic);
        if older_clear {
            delete_intent(tx, &chat.target, MARKED_UNREAD)?;
            let told = mirror_of(tx, account, &chat, now)?;
            set_mirror(tx, &chat, told, Some(true), now)?;
        }
        let mut patch = ConversationPatch::new(chat.conv.clone());
        patch.marked_unread = Some(true);
        append_events(tx, &[BackendEvent::ConversationUpsert(patch)], now, wake)?;
        return Ok(Automatic::PhoneKept);
    }
    // N-D: there is something to clear only when the phone, or a click not written yet, says
    // marked.
    let marked = live
        .as_ref()
        .map_or(flags.marked_unread, |l| l.value == Some(1));
    if !marked {
        return Ok(Automatic::Dropped);
    }
    file(
        tx,
        &Fresh {
            target: &chat.target,
            field: MARKED_UNREAD,
            value: flag_value(false),
            base: flag_value(flags.marked_unread),
            shown: flag_value(false),
            key: None,
            automatic: true,
        },
        now,
    )?;
    let told = mirror_of(tx, account, &chat, now)?;
    set_mirror(tx, &chat, told, Some(false), now)?;
    schedule_write(tx, now)?;
    Ok(Automatic::Filed)
}

// ---------------------------------------------------------------------------------------------
// A safety number's state (P5a)
// ---------------------------------------------------------------------------------------------

/// What [`intend_identity`] did, and what the phone's record of the person says of the key
/// (A10c C5b).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdentityFiled {
    /// An intent was filed, and the newest stored record holds the SAME key: the write carries the
    /// state onto it, so the contact record tells the other devices.
    SameKey,
    /// An intent was filed, but the newest stored record holds ANOTHER key or none: the plan
    /// states `identityState` only onto a record of the same key (`Why::OtherKey`, M2), so the
    /// write is most likely to drop the intent. The caller also tells the other devices through
    /// the Verified sync, which carries the same state for the same key.
    OtherKey,
    /// Nothing was filed: the switch is off, or the phone has no contact record of the person.
    NotFiled,
}

#[cfg(test)]
impl IdentityFiled {
    /// Whether an intent was filed (the state of the key in the record aside).
    pub(crate) const fn is_filed(self) -> bool {
        !matches!(self, Self::NotFiled)
    }
}

/// A Mark / Clear / Confirm of the safety number of the person `sid` (an ACI string): the
/// `(contact:<sid>, identity)` intent with the key the click was made for (critic M2), when the
/// switch is on and the phone has a record of the person. `state` is `identities.verified`
/// (0 DEFAULT, 1 VERIFIED, 2 UNVERIFIED); `previous` the state before the click. Says whether an
/// intent was filed and whether the record it was filed against holds the SAME key ([`IdentityFiled`]);
/// a switch that is off or a person without a record is no error.
pub(crate) fn intend_identity(
    tx: &mut Tx<'_>,
    account: &Account,
    sid: &str,
    state: i64,
    previous: i64,
    key: &[u8],
    now: i64,
) -> Result<IdentityFiled> {
    if !switch_is_on(tx)? {
        return Ok(IdentityFiled::NotFiled);
    }
    fill_targets(tx, account)?;
    let target = contact_target(sid);
    let Stored::Found {
        record: proto::StorageRecord {
            record: Some(Record::Contact(contact)),
        },
        ..
    } = newest_record(tx, &target)?
    else {
        return Ok(IdentityFiled::NotFiled);
    };
    let same_key = !contact.identity_key.is_empty() && contact.identity_key == key;
    file(
        tx,
        &Fresh {
            target: &target,
            field: IDENTITY,
            value: Some(state),
            // The base is `previous`, the state the key had HERE before the click, for a record
            // of the same key too (A10c re-check N4): every sync applied and every record merged
            // writes that state, so it is at least as new as the stored record's, which can
            // still say what the phone held before a Verified sync that was applied here
            // (a base taken from the record would make that phone's click look untouched and
            // the NEWER click here lose to the record of the older one). A write in flight that
            // carries the field still wins in `file` (X3). Review M1: the phone's state is the
            // state OF A KEY; a record that holds another key (or none) says nothing about this
            // one, and the phone's state of the new key is the one it derived from the key
            // change, by the rule Kourier follows too (a new key after DEFAULT is DEFAULT,
            // after VERIFIED or UNVERIFIED it is UNVERIFIED: Android's
            // `SignalBaseIdentityKeyStore.saveIdentity`, `store/identity.rs` `save_identity`),
            // so (review N3) the phone's automatic state of the new key is `theirs == base`
            // (the click is newer and stays), its own Verify of it is `theirs != base` (it wins).
            base: Some(previous),
            shown: Some(previous),
            key: Some(key),
            automatic: false,
        },
        now,
    )?;
    schedule_write(tx, now)?;
    Ok(if same_key {
        IdentityFiled::SameKey
    } else {
        IdentityFiled::OtherKey
    })
}

/// The time of the click (`created_at`, ms) that the live identity intent of the person `sid`
/// stands for, when that intent is for exactly `key` (V1: the intent takes part in P5a's "latest
/// click wins by time" as the queued `VerifiedSync` job did). A read; an intent for another key
/// says nothing about this one.
pub(crate) fn identity_click_at(conn: &Connection, sid: &str, key: &[u8]) -> Result<Option<i64>> {
    Ok(load_intent(conn, &contact_target(sid), IDENTITY)?
        .filter(|intent| intent.key.as_deref() == Some(key))
        .map(|intent| intent.created_at))
}

/// A phone's Verified sync NEWER than the live identity intent of `sid` was applied (V1,
/// `safety::apply_remote_state`): the click is overruled, the intent is deleted (the apply states
/// the new state, nothing else is restated). When a write in flight CARRIES the identity entry
/// that write still puts the click's state on the phone, so the phone's newer `state` is filed as
/// a NEW intent after it (X3's shape: its `base` is the value the write sets) and the next write
/// puts it back. Only with the switch on (with it off no intent is filed) and only when the
/// carried value is another state. `click_at` is the sync's own timestamp, the phone's click: the
/// refiled intent is stamped with it, not with the time the sync was received (`now`, which only
/// schedules the write), so a phone sync that arrives later but is older than the receipt (the
/// phone's next click, a catch-up after a sleep) compares against the phone's click and is not
/// `Kept` by our receipt time (A10c re-check N1).
pub(crate) fn retire_identity(
    conn: &Connection,
    sid: &str,
    key: &[u8],
    state: i64,
    click_at: i64,
    now: i64,
) -> Result<()> {
    let target = contact_target(sid);
    delete_intent(conn, &target, IDENTITY)?;
    let Some(carried) = carried_by_inflight(conn, &target, IDENTITY)? else {
        return Ok(());
    };
    if carried.value == Some(state) || !switch_is_on(conn)? {
        return Ok(());
    }
    file(
        conn,
        &Fresh {
            target: &target,
            field: IDENTITY,
            value: Some(state),
            base: carried.value,
            shown: carried.value,
            key: Some(key),
            automatic: false,
        },
        click_at,
    )?;
    schedule_write(conn, now)
}

/// The live identity intent of `target` that may be written onto a record holding `record_key`:
/// the same key, or a record with none. An intent for ANOTHER key is dropped on the spot and
/// nothing is stated (the phone's key stands, the next profile fetch decides): a state set for a
/// key the record does not hold would be written onto the wrong person (critic M2).
pub(crate) fn identity_for(
    conn: &Connection,
    target: &str,
    record_key: &[u8],
) -> Result<Option<Intent>> {
    let Some(intent) = load_intent(conn, target, IDENTITY)? else {
        return Ok(None);
    };
    if record_key.is_empty() || intent.key.as_deref() == Some(record_key) {
        return Ok(Some(intent));
    }
    delete_intent(conn, target, IDENTITY)?;
    Ok(None)
}

// ---------------------------------------------------------------------------------------------
// Dropping intents
// ---------------------------------------------------------------------------------------------

/// The switch went off (BQ10, Y6): deletes the intents that no write in flight carries, leaves
/// the in-flight row, and restates the phone's values of what it dropped to the core (each
/// intent's `base`, review H2). A field the write in flight carries with an older `seq` (a click
/// made during the PUT), or as X1's ride-along, is deleted without a restatement: the landing
/// states what was written. Returns how many intents it deleted.
pub(crate) fn drop_all(
    tx: &mut Tx<'_>,
    account: &Account,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<usize> {
    drop_where(tx, account, now, wake, |_| true, |_| true)
}

/// Deletes the intents older than [`STALE_AFTER_MS`] with the same restatement (Y3 (3)) and one
/// warn line (a count). A10c's job calls it. Returns how many it deleted.
pub(crate) fn drop_stale(
    tx: &mut Tx<'_>,
    account: &Account,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<usize> {
    let cutoff = now.saturating_sub(STALE_AFTER_MS);
    let dropped = drop_where(
        tx,
        account,
        now,
        wake,
        |intent| intent.created_at < cutoff,
        |_| true,
    )?;
    if dropped > 0 {
        tracing::warn!(
            dropped,
            "storage intents older than 30 days were dropped and the phone's values restated"
        );
    }
    Ok(dropped)
}

/// Deletes the intents `wanted` picks (a write in flight keeps the one it carries) and restates
/// the phone's values (see [`drop_all`]). V2: a dropped `identity` intent that `resync` accepts
/// becomes the person's `VerifiedSync` (its `value`, `key` and `created_at` as `set_at`): neither
/// the write nor anything else would tell the other devices of the click now. Not an intent that
/// ends in the three-way rule or by a landing (they never come here), nor one the plan dropped as
/// `OtherKey` (the sync was queued with the click, C5b).
fn drop_where(
    tx: &mut Tx<'_>,
    account: &Account,
    now: i64,
    wake: &Rc<Notify>,
    wanted: impl Fn(&Intent) -> bool,
    resync: impl Fn(&Intent) -> bool,
) -> Result<usize> {
    let in_flight = planned(tx)?;
    // (intent, restate its stored value)
    let mut doomed: Vec<(Intent, bool)> = Vec::new();
    for intent in all_intents(tx)?.into_iter().filter(|i| wanted(i)) {
        match in_flight
            .iter()
            .find(|p| p.target == intent.target && p.field == intent.field)
        {
            // The write in flight carries exactly this intent: it lands, and the landing closes it.
            Some(p) if !p.rides && p.seq == intent.seq => {}
            // A later click on a carried field (X1's ride-along included, review H1): the landing
            // states the written value (D2: marked in the row below, so it does whatever the
            // switch says by then).
            Some(_) => doomed.push((intent, false)),
            None => doomed.push((intent, true)),
        }
    }
    for (intent, _) in &doomed {
        delete_intent(tx, &intent.target, &intent.field)?;
    }
    let unstated: Vec<(&str, &str)> = doomed
        .iter()
        .filter(|(_, restate)| !*restate)
        .map(|(intent, _)| (intent.target.as_str(), intent.field.as_str()))
        .collect();
    if !unstated.is_empty() {
        write::mark_state_at_landing(tx, &unstated)?;
    }
    for (intent, _) in doomed.iter().filter(|(intent, _)| resync(intent)) {
        if let (IDENTITY, Some(sid), Some(state), Some(key)) = (
            intent.field.as_str(),
            contact_sid(&intent.target),
            intent.value,
            intent.key.as_deref(),
        ) {
            crate::safety::queue_verified_sync(tx, sid, state, key, intent.created_at, now)?;
        }
    }
    fill_targets(tx, account)?;
    let mut events: Vec<BackendEvent> = Vec::new();
    let mut targets: Vec<&str> = Vec::new();
    for (intent, restate) in &doomed {
        if *restate && !targets.contains(&intent.target.as_str()) {
            targets.push(&intent.target);
        }
    }
    let mut pins_dropped = false;
    for target in targets {
        let dropped: Vec<&Intent> = doomed
            .iter()
            .filter(|(i, restate)| *restate && i.target == target)
            .map(|(i, _)| i)
            .collect();
        if dropped.iter().any(|i| i.field.starts_with(PIN_PREFIX)) {
            pins_dropped = true;
        }
        let Some(chat) = chat_of_target(account, target) else {
            continue;
        };
        let mut patch = ConversationPatch::new(chat.conv.clone());
        let mut patched = false;
        let mut told = mirror_of(tx, account, &chat, now)?;
        let mut marked = None;
        // What the phone has is the intent's `base` (review H2): the stored record's value when
        // the click was made, except where A1's told base (an archive) or a landing (the written
        // value) moved it, and a record that differs from it was merged with the rule, which
        // closes the intent. The record itself is not read again.
        for intent in dropped {
            match intent.field.as_str() {
                ARCHIVED => {
                    let archived = intent.base == Some(1);
                    patch.archived = Some(archived);
                    told.archived = archived;
                    patched = true;
                }
                MARKED_UNREAD => {
                    let phone_marked = intent.base == Some(1);
                    patch.marked_unread = Some(phone_marked);
                    marked = Some(phone_marked);
                    patched = true;
                }
                MUTED_UNTIL => {
                    patch.muted_until = intent.base.map_or(Patch::Clear, Patch::Set);
                    told.muted_until = intent.base;
                    patched = true;
                }
                _ => {}
            }
        }
        if patched {
            set_mirror(tx, &chat, told, marked, now)?;
            events.push(BackendEvent::ConversationUpsert(patch));
        }
    }
    if pins_dropped && let Some(pins) = Pins::load(tx)? {
        events.push(pins.list_event(tx)?);
    }
    if !events.is_empty() {
        append_events(tx, &events, now, wake)?;
    }
    Ok(doomed.len())
}

/// What the plan of a write found can never be written (`write::Plan::left`: `NotWritable`,
/// `OtherKey`, `OverPinLimit`), in the zone of the plan: those intents are deleted, the phone's
/// value of a flag is stated as for any dropped intent ([`drop_where`]: the intent's `base`) and,
/// for a pin among them, the WHOLE pin list (the stored record with the pin intents that are left
/// applied: a rebase over the limit drops our newest addition, M9 b). A safety number's state has
/// nothing to state (M2). Only the very intents the plan looked at go (same `seq`). The other
/// reasons (`NotListed`, `Unreadable`) keep the intent: a later record decides. Returns how many
/// it deleted.
pub(crate) fn drop_left(
    tx: &mut Tx<'_>,
    account: &Account,
    left: &[write::Left],
    now: i64,
    wake: &Rc<Notify>,
) -> Result<usize> {
    let doomed: Vec<&write::Left> = left.iter().filter(|l| !l.why.keeps_the_intent()).collect();
    if doomed.is_empty() {
        return Ok(0);
    }
    let is_left = |l: &&write::Left, intent: &Intent| {
        l.target == intent.target && l.field == intent.field && l.seq == intent.seq
    };
    drop_where(
        tx,
        account,
        now,
        wake,
        |intent| doomed.iter().any(|l| is_left(l, intent)),
        // `OtherKey`: C5b queued the sync with the click already.
        |intent| {
            !doomed
                .iter()
                .any(|l| is_left(l, intent) && l.why == write::Why::OtherKey)
        },
    )
}

/// A1's zone moved the chat out of the archive (`receive/unarchive.rs` `leaves_the_archive`,
/// called when a message brings it back; critic X1, BQ1 (a)). What the write in flight sets for
/// the chat's `archived` decides (an Archive intent it carries, or X1's ride-along of a told base
/// that said archived, review H1):
/// - It archives the chat on the phone: a NEW intent `archived = false` follows it (`base` 1, the
///   value the write sets), whether or not an intent of ours asked for the `1`, unless the switch
///   is OFF: such an intent would never be written (the job writes nothing then) and would linger
///   until the 30-day drop, so nothing is filed (B6 open item 1).
/// - Nothing in flight archives it: a live Archive intent not written yet is overtaken and
///   deleted.
///
/// Either way EVERY `archived` entry the write in flight carries for the chat is marked overtaken
/// ([`Planned::overtaken`], [`write::mark_overtaken`]; review L1 and R2-2 (1)): whatever it sets
/// (an Archive's 1, an Unarchive's 0, a ride-along), the message told the core `archived = false`,
/// so a landing with the switch off does not state it and a write that does not land does not
/// restate its `prior` over it.
///
/// A live Unarchive intent is left as it is (the click already told the core `archived = false`;
/// known limit R2-2 (2)). The core is told by the unarchive upsert the caller already emits.
pub(crate) fn chat_left_the_archive(conn: &Connection, target: &str, now: i64) -> Result<()> {
    let live = load_intent(conn, target, ARCHIVED)?;
    if live.as_ref().is_some_and(|live| live.value != Some(1)) {
        return Ok(());
    }
    let in_flight_archives = carried_by_inflight(conn, target, ARCHIVED)?
        .is_some_and(|planned| planned.value == Some(1));
    // Any carried `archived` entry of the chat is overtaken, not only one that sets 1 (review
    // R2-2 (1)): the message told the core `archived = false`, and a not landed write with the
    // switch off must not restate the entry's `prior` (an Unarchive's 1) over it.
    write::mark_overtaken(conn, target)?;
    if !in_flight_archives {
        if live.is_some() {
            delete_intent(conn, target, ARCHIVED)?;
        }
        return Ok(());
    }
    if !switch_is_on(conn)? {
        return Ok(());
    }
    let seq = next_seq(conn)?;
    put_intent(
        conn,
        &Intent {
            target: target.to_owned(),
            field: ARCHIVED.to_owned(),
            value: flag_value(false),
            base: flag_value(true),
            shown: flag_value(true),
            key: None,
            seq,
            automatic: false,
            created_at: now,
        },
    )?;
    schedule_write(conn, now)
}

// ---------------------------------------------------------------------------------------------
// Landing (§3.6; critic X3, X4, Y5, Y6, M4) and what a rebuilt core.db is told
// ---------------------------------------------------------------------------------------------

/// What [`land`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Landing {
    /// The write landed. `closed`: the carried intents deleted (their `seq` still equalled the
    /// carried one). `moved`: the intents of a carried field that a NEWER click replaced meanwhile,
    /// kept, with `base` moved to the value the write set (H3).
    Landed { closed: usize, moved: usize },
    /// `account.storage_generation` moved since the write was planned (a `storage::reset` ran,
    /// M4): the result is dropped and the in-flight row is gone. The write DID land (only a 200
    /// reaches this), so it is NOT a write that did not land (the lead's call, round 3): no
    /// intent's `base` or `shown` goes back to `prior` (the full sync after the reset merges the
    /// phone's records, ours included, and a newer click's base must stay the written value), and
    /// with the switch OFF every intent is dropped and the phone's values restated ([`drop_all`],
    /// review M2).
    Stale,
    /// [`land_written`] found the write already judged (review N4): the stored in-flight row is
    /// not this write (a sync judged it, or there is none), or its records could not be stored (a
    /// sync merged its version or a newer one, or the plan is not the row). Nothing is done.
    AlreadyJudged,
}

/// What [`resolve_in_flight`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// No write is in flight.
    NoRow,
    /// A write was in flight and did not land (or its row could not be read, review L3): the row
    /// is deleted and [`not_landed`] runs: the bases a click took from the write go back to the
    /// phone's value (review N1 (b)) and the write is planned again. With the switch OFF it is
    /// not: every intent is dropped and the phone's values are restated (reviews M2, N1).
    NotLanded,
    /// A write was in flight and landed. `assumed`: the manifest moved past the write and lists
    /// none of its identifiers ([`Landed::Presumed`]).
    Found { landing: Landing, assumed: bool },
}

pub(super) fn storage_generation(conn: &Connection) -> Result<i64> {
    Ok(conn
        .prepare_cached("SELECT storage_generation FROM account WHERE id = 1")?
        .query_row([], |r| r.get(0))?)
}

fn move_base(conn: &Connection, target: &str, field: &str, base: Option<i64>) -> Result<()> {
    conn.prepare_cached("UPDATE storage_intents SET base = ?3 WHERE target = ?1 AND field = ?2")?
        .execute((target, field, base))?;
    Ok(())
}

/// A write that did not land gives back what it lent a newer click (review N1 (b), R2-1): the
/// `base` AND the `shown` of the intent become `prior`, the phone's value before the write. The
/// `shown` the click took from the carried one was a value the phone never got: a click back to
/// it must file a change, not undo (L6) one that was never written.
fn give_back(conn: &Connection, target: &str, field: &str, prior: Option<i64>) -> Result<()> {
    conn.prepare_cached(
        "UPDATE storage_intents SET base = ?3, shown = ?3 WHERE target = ?1 AND field = ?2",
    )?
    .execute((target, field, prior))?;
    Ok(())
}

/// Whether a ride-along's `prior` (the `archived` the phone had before the write, review R2-2
/// (3)) is still what the newest stored record of `target` says. A newer record (a merge since
/// the plan stored it and stated its value) says another value: that newer value stands, and
/// restating `prior` over it would undo the phone. With no readable record nothing contradicts
/// `prior`. (The told base is not the test: a click that `drop_all` deleted unstated moved it, and
/// that is the very value a write that did not land must replace, N2.)
fn prior_stands(
    conn: &Connection,
    account: &Account,
    target: &str,
    prior: Option<i64>,
) -> Result<bool> {
    let Some(chat) = chat_of_target(account, target) else {
        return Ok(true);
    };
    Ok(match newest_record(conn, target)? {
        Stored::Found { record, .. } => {
            flags_of(&chat, &record).is_none_or(|flags| Some(i64::from(flags.archived)) == prior)
        }
        Stored::Missing | Stored::Unreadable => true,
    })
}

/// THE LANDING of a write: what closes its intents. Two paths call it, and in both NO command can
/// run between the landing and the moment our own records are what the stored records say:
/// - the 200 path (the PUT answered 200) through [`land_written`], which stores the written
///   records and the version in the same zone first, so no sync ever merges them as the phone's;
/// - a write found in flight after a restart ([`resolve_in_flight`]), inside the FIRST merge zone
///   of the fetched manifest that judges it, before that zone's merge (`storage::sync`; the merge
///   zones run back to back with no await between them). Alone, without the merge right after it,
///   it would leave the stored records the write replaced in place while the intents are closed
///   (review B1): a click, a reading or an unpin in that window would take the replaced record's
///   value as the phone's and lose to the merge of our own record.
///
/// What it does (`row` is the write that landed):
/// - For each `(target, field)` the write carried: the intent whose `seq` still equals the carried
///   one is deleted (Y5: the write closed exactly that click); an intent a NEWER click put there
///   stays and its `base` becomes the written value (H3), so the copy of our own record
///   (`theirs == base`) leaves it alone. A ride-along (X1's told `archived`, [`Planned::rides`])
///   closes no intent: it only moves the base of a live archive intent.
/// - It stores nothing of the records itself ([`land_written`] does, before it).
/// - With the switch OFF it still records the landing and tells the core the WRITTEN values (Y6):
///   what was on its way when the switch went off still arrives, and `drop_all` deliberately
///   stated nothing for it, except a written `archived = 1` whose chat a message brought back
///   since the write was planned ([`Planned::overtaken`], review L1). With the switch on the core
///   already shows ours.
/// - The in-flight row is deleted. A generation that moved drops the result ([`Landing::Stale`]):
///   the row is deleted and, with the switch OFF only, [`drop_all`] runs (review M2). The write
///   landed, so it is not [`not_landed`]: no base or `shown` is put back (round 3).
pub(crate) fn land(
    tx: &mut Tx<'_>,
    account: &Account,
    row: &InFlight,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<Landing> {
    if storage_generation(tx)? != row.generation {
        // Stale is not "not landed" (round 3): only a 200 gets here, so the write is on the server
        // and the full sync after the reset merges it with the phone's records. A newer click's
        // base stays the value the write set (X3, H3); nothing is put back to `prior`.
        write::delete_in_flight(tx)?;
        if !switch_is_on(tx)? {
            drop_all(tx, account, now, wake)?;
        }
        return Ok(Landing::Stale);
    }
    // The stored row of the same write is the one to land: a message may have marked an entry of
    // it since it was written ([`Planned::overtaken`]); the 200 path hands in the row it wrote.
    let stored = write::read_in_flight(tx)?;
    let row = match &stored {
        Some(stored) if stored.version == row.version && stored.new_keys == row.new_keys => stored,
        _ => row,
    };
    let (mut closed, mut moved) = (0, 0);
    for carried in &row.planned {
        match load_intent(tx, &carried.target, &carried.field)? {
            None => {}
            Some(live) if !carried.rides && live.seq == carried.seq => {
                delete_intent(tx, &carried.target, &carried.field)?;
                closed += 1;
            }
            Some(_) => {
                move_base(tx, &carried.target, &carried.field, carried.value)?;
                moved += 1;
            }
        }
    }
    // With the switch OFF the core is told every value the write set (Y6). With it on the core
    // already shows ours, except where a newer click on a carried field was deleted while the
    // switch was off (D2: `state_at_landing`, no live intent since): the landing states those.
    let events = state_written(tx, account, row, now, switch_is_on(tx)?)?;
    if !events.is_empty() {
        append_events(tx, &events, now, wake)?;
    }
    write::delete_in_flight(tx)?;
    tracing::debug!(closed, moved, "a storage write landed");
    Ok(Landing::Landed { closed, moved })
}

/// THE 200 ZONE's landing (review B1; A10c's write job calls it in the zone of a 200, with the
/// in-flight row it wrote and the plan that row was made of): in ONE transaction the written
/// records replace the old ones in `storage_records` and `manifest_version` becomes the written
/// version ([`write::store_written`]), then [`land`] closes the intents. Every reader that comes
/// after it (a click's `base`, the automatic clear, the pin list) sees what the phone now has, and
/// the next sync of that version finds nothing new to merge. A generation that moved (a
/// `storage::reset` during the PUT) stores nothing and lands [`Landing::Stale`]. It lands ONLY
/// its own write (review N4): when the stored in-flight row is not this write (a sync judged it
/// first: the write job and `storage_sync` never overlap in A10c, and this guard covers it if
/// they do, or a sync judged it NOT landed because the phone wrote its very version: that guard
/// decides alone there) or the records could not be stored (`AlreadyMerged`, `NotTheRow`), nothing is done
/// ([`Landing::AlreadyJudged`]): a second landing would move a newer click's base to our older
/// written value and, with the switch off, state that value over the phone's newer one.
pub(crate) fn land_written(
    tx: &mut Tx<'_>,
    account: &Account,
    row: &InFlight,
    plan: &write::Plan,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<Landing> {
    if storage_generation(tx)? != row.generation {
        return land(tx, account, row, now, wake);
    }
    let own = write::read_in_flight(tx)?
        .is_some_and(|stored| stored.version == row.version && stored.new_keys == row.new_keys);
    if !own {
        return Ok(Landing::AlreadyJudged);
    }
    match write::store_written(tx, row, plan, now)? {
        write::Written::Stored | write::Written::StoredBehind => land(tx, account, row, now, wake),
        write::Written::AlreadyMerged | write::Written::NotTheRow => Ok(Landing::AlreadyJudged),
    }
}

/// EVERY exit of a write that did NOT land (review N1 (b), the lead's call: a write found not
/// landed, an unreadable row, and A10c's non-2xx through [`write_not_landed`]; NOT a stale
/// generation: that write landed, see [`Landing::Stale`]); the caller deleted the row and hands
/// in what it carried (`planned`; empty for a row this build could not read: nothing can be put
/// back then).
/// - The inverse of H3: a live intent on a carried field that is not the carried one (a click
///   filed while the row existed, X3, or a live archive intent beside X1's ride-along) took the
///   written value as its `base` and as its `shown` (what the core showed was the carried
///   click's value); nothing was written, so BOTH go back to [`Planned::prior`], the phone's
///   value before the write (review R2-1: a click back to the failed write's value is then a new
///   intent, not an undo of nothing). With the switch on that is all: the intents stay and the
///   next plan writes them again.
/// - With the switch OFF nothing carries the intents any more and nothing will ever write them:
///   every intent is dropped and its `base` restated ([`drop_all`]; review M2), and each carried
///   field whose newer click `drop_all` deleted UNSTATED while the row lived (Y6: the landing was
///   to state the written value) is restated as `prior` too (nothing landed, the phone holds
///   it; reviews M2's variant and N2), the pin list once for the pins among them. Not an
///   `archived` entry a message overtook ([`Planned::overtaken`]: A1 told the core already), and
///   a ride-along's `prior` only while the newest stored record of the chat still says it
///   ([`prior_stands`], review R2-2 (3): another value there is a newer one a merge stated).
fn not_landed(
    tx: &mut Tx<'_>,
    account: &Account,
    planned: &[Planned],
    now: i64,
    wake: &Rc<Notify>,
) -> Result<()> {
    let mut unstated: Vec<&Planned> = Vec::new();
    for carried in planned {
        match load_intent(tx, &carried.target, &carried.field)? {
            Some(live) if carried.rides || live.seq != carried.seq => {
                give_back(tx, &carried.target, &carried.field, carried.prior)?;
            }
            Some(_) => {}
            None => unstated.push(carried),
        }
    }
    if switch_is_on(tx)? {
        // D2: the fields whose newer click was deleted while the switch was off, with the landing
        // to state the written value (`state_at_landing`): nothing was written, so the phone's
        // `prior` is stated instead. Not a field a live intent holds again (`unstated` has none).
        let owed: Vec<&Planned> = unstated
            .iter()
            .copied()
            .filter(|p| p.state_at_landing)
            .collect();
        let events = state_priors(tx, account, &owed, now)?;
        if !events.is_empty() {
            append_events(tx, &events, now, wake)?;
        }
        return Ok(());
    }
    drop_all(tx, account, now, wake)?;
    let events = state_priors(tx, account, &unstated, now)?;
    if !events.is_empty() {
        append_events(tx, &events, now, wake)?;
    }
    Ok(())
}

/// What the core is told when a write did not land: the phone's value before it (`prior`) of each
/// carried field in `entries` (fields no live intent holds), the pin list once for the pins among
/// them. Not an `archived` entry a message overtook ([`Planned::overtaken`]: A1 told the core
/// already), and a ride-along's `prior` only while the newest stored record of the chat still says
/// it ([`prior_stands`], review R2-2 (3): another value there is a newer one a merge stated).
fn state_priors(
    tx: &mut Tx<'_>,
    account: &Account,
    entries: &[&Planned],
    now: i64,
) -> Result<Vec<BackendEvent>> {
    let mut priors: Vec<(&str, &str, Option<i64>)> = Vec::new();
    for p in entries.iter().filter(|p| !p.overtaken) {
        // (3): a ride-along's `prior` is the phone's value at the plan. A newer record says
        // another one (a merge stated it): it stands.
        if p.rides && !prior_stands(tx, account, &p.target, p.prior)? {
            continue;
        }
        priors.push((p.target.as_str(), p.field.as_str(), p.prior));
    }
    let mut events = state_values(tx, account, &priors, now)?;
    if entries
        .iter()
        .any(|p| p.target == ACCOUNT_TARGET && p.field.starts_with(PIN_PREFIX))
        && let Some(pins) = Pins::load(tx)?
    {
        events.push(pins.list_event(tx)?);
    }
    Ok(events)
}

/// A10c's non-2xx path (a 409, or an answer after which the write is known not to be on the
/// server), in the zone of that answer with the row it wrote: the stored in-flight row of the
/// SAME write (version and new identifiers; it may carry a mark made since, as in [`land`]) is
/// deleted and [`not_landed`] runs. No such row: a sync judged it already, nothing is done.
/// Returns whether it judged the write.
pub(crate) fn write_not_landed(
    tx: &mut Tx<'_>,
    account: &Account,
    row: &InFlight,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<bool> {
    let Some(stored) = write::read_in_flight(tx)?
        .filter(|stored| stored.version == row.version && stored.new_keys == row.new_keys)
    else {
        return Ok(false);
    };
    write::delete_in_flight(tx)?;
    not_landed(tx, account, &stored.planned, now, wake)?;
    Ok(true)
}

/// What [`judge`] found.
enum Judged {
    NoRow,
    /// The fetched manifest is behind the row's version and the caller leaves such a row to A10c's
    /// job (review R2-3): nothing was done.
    Behind,
    /// Not landed (or unreadable): already resolved by [`not_landed`].
    NotLanded,
    Landed(InFlight, Landed),
}

/// The landing rule on the row in flight: a write that did not land (or a row this build cannot
/// read, review L3) is resolved here (the row deleted, [`not_landed`]); a landed one is handed
/// back for [`land`]. With `leave_behind`, a row whose version is above the fetched manifest's is
/// left alone ([`Judged::Behind`], review R2-3): a manifest behind the write proves nothing about
/// it (the PUT may be on the wire, or answered after the server applied it).
fn judge(
    tx: &mut Tx<'_>,
    account: &Account,
    fetched: &proto::ManifestRecord,
    leave_behind: bool,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<Judged> {
    let row = match write::find_in_flight(tx)? {
        write::Row::None => return Ok(Judged::NoRow),
        write::Row::Readable(row) => row,
        // Review L3: a row this build cannot read is a write that did not land (the row is
        // already deleted and logged); nothing of it can be put back.
        write::Row::Unreadable => {
            not_landed(tx, account, &[], now, wake)?;
            return Ok(Judged::NotLanded);
        }
    };
    if leave_behind && fetched.version < row.version {
        return Ok(Judged::Behind);
    }
    // The ids the write deleted are read BEFORE any merge zone stores the manifest judged here
    // (D1): both judges (the sync's and the job's) come through this function.
    let deleted = write::deleted_ids(tx, &row)?;
    let verdict = write::landed(fetched, &row, &deleted);
    if !verdict.is_landed() {
        write::delete_in_flight(tx)?;
        not_landed(tx, account, &row.planned, now, wake)?;
        return Ok(Judged::NotLanded);
    }
    Ok(Judged::Landed(row, verdict))
}

/// Review N1 (a): `storage::sync` judges a write in flight right after the fetched manifest
/// decrypts, BEFORE it reads the items (`write::landed` needs only the manifest): a write that
/// did NOT land, or a row this build cannot read, is resolved in this zone of its own, so a click
/// made while the items are read finds no row and takes the phone's value as its base, as before
/// the fix round. A landed write is left for the first merge zone ([`resolve_in_flight`], review
/// B1). A row whose version is ABOVE the fetched manifest's is left alone too (review R2-3): a
/// behind manifest reads no items, so N1 (a) gains nothing from judging it, and while a PUT is on
/// the wire it would delete the row of a write the server then takes (the 200 would find no row).
/// A10c's job judges that case. The proof that stays: the manifest of the write's own version
/// without any of our identifiers (the phone wrote that version). Returns whether it resolved a
/// row.
pub(crate) fn resolve_not_landed(
    tx: &mut Tx<'_>,
    account: &Account,
    fetched: &proto::ManifestRecord,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<bool> {
    Ok(matches!(
        judge(tx, account, fetched, true, now, wake)?,
        Judged::NotLanded
    ))
}

/// A write found in flight (at the start, or by a job that fetched the manifest): judges it by
/// the FETCHED manifest ([`write::landed`]) and either runs the landing ([`land`]) or deletes the
/// row ([`not_landed`]). `storage::sync` resolves a write that did not land before it reads the
/// items ([`resolve_not_landed`], review N1 (a)) and calls this inside the first merge zone of
/// that manifest, before its merge, with no await between it and the last merge zone (review
/// B1): the landing stores none of our records, the merge of that version does. A moved
/// manifest with none of our identifiers counts as landed and is logged as a count (BQ9).
pub(crate) fn resolve_in_flight(
    tx: &mut Tx<'_>,
    account: &Account,
    fetched: &proto::ManifestRecord,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<Resolved> {
    let (row, verdict) = match judge(tx, account, fetched, false, now, wake)? {
        Judged::NoRow | Judged::Behind => return Ok(Resolved::NoRow),
        Judged::NotLanded => return Ok(Resolved::NotLanded),
        Judged::Landed(row, verdict) => (row, verdict),
    };
    let assumed = verdict == Landed::Presumed;
    if assumed {
        tracing::warn!(
            carried = row.planned.len(),
            "the storage manifest moved past a write that left no trace: counted as landed"
        );
    }
    let landing = land(tx, account, &row, now, wake)?;
    Ok(Resolved::Found { landing, assumed })
}

/// The events that tell the core the values `values` = `(target, field, value)` give the flags of
/// their chats (`archived`, `marked_unread`, `muted_until`; other fields are skipped), with the
/// told base of each chat moved to them: the shape of the restatements in [`drop_where`].
fn state_values(
    conn: &Connection,
    account: &Account,
    values: &[(&str, &str, Option<i64>)],
    now: i64,
) -> Result<Vec<BackendEvent>> {
    let is_flag = |field: &str| matches!(field, ARCHIVED | MARKED_UNREAD | MUTED_UNTIL);
    let mut targets: Vec<&str> = Vec::new();
    for (target, field, _) in values {
        if is_flag(field) && !targets.contains(target) {
            targets.push(target);
        }
    }
    let mut events = Vec::new();
    for target in targets {
        let Some(chat) = chat_of_target(account, target) else {
            continue;
        };
        let mut patch = ConversationPatch::new(chat.conv.clone());
        let mut told = mirror_of(conn, account, &chat, now)?;
        let mut marked = None;
        for (_, field, value) in values.iter().filter(|(t, ..)| *t == target) {
            match *field {
                ARCHIVED => {
                    patch.archived = Some(*value == Some(1));
                    told.archived = *value == Some(1);
                }
                MARKED_UNREAD => {
                    patch.marked_unread = Some(*value == Some(1));
                    marked = Some(*value == Some(1));
                }
                MUTED_UNTIL => {
                    patch.muted_until = value.map_or(Patch::Clear, Patch::Set);
                    told.muted_until = *value;
                }
                _ => {}
            }
        }
        set_mirror(conn, &chat, told, marked, now)?;
        events.push(BackendEvent::ConversationUpsert(patch));
    }
    Ok(events)
}

/// Y6: what the core is told when a write lands with the switch off: every value the write set
/// (the flags per chat, and the pin list as the stored account record plus the pins the write
/// carried, the removals dropped and the additions last). Not a written `archived = 1` that a
/// message overtook after the write was planned ([`Planned::overtaken`], review L1): the chat
/// with the new message stays in the inbox here. `owed_only` (the switch is ON, D2): the core
/// already shows ours, so only the entries a newer click was deleted from while the switch was off
/// ([`Planned::state_at_landing`]) and that no live intent holds now are stated, with the live pin
/// intents on top of the pin list.
fn state_written(
    conn: &Connection,
    account: &Account,
    row: &InFlight,
    now: i64,
    owed_only: bool,
) -> Result<Vec<BackendEvent>> {
    let mut stated: Vec<&Planned> = Vec::new();
    for p in row.planned.iter().filter(|p| !p.overtaken) {
        if owed_only && (!p.state_at_landing || load_intent(conn, &p.target, &p.field)?.is_some()) {
            continue;
        }
        stated.push(p);
    }
    let values: Vec<(&str, &str, Option<i64>)> = stated
        .iter()
        .map(|p| (p.target.as_str(), p.field.as_str(), p.value))
        .collect();
    let mut events = state_values(conn, account, &values, now)?;
    let is_pin = |p: &Planned| p.target == ACCOUNT_TARGET && p.field.starts_with(PIN_PREFIX);
    if stated.iter().any(|p| is_pin(p))
        && let Some(pins) = Pins::load(conn)?
    {
        // The pins the write carried (a field a live intent holds is the live intent's), then the
        // live pin intents; with the switch off there are none of those.
        let live = pins.intents;
        let mut intents: Vec<Intent> = row
            .planned
            .iter()
            .filter(|p| is_pin(p) && !live.iter().any(|l| l.field == p.field))
            .map(|p| Intent {
                target: p.target.clone(),
                field: p.field.clone(),
                value: p.value,
                base: None,
                shown: None,
                key: None,
                seq: p.seq,
                automatic: false,
                created_at: now,
            })
            .collect();
        intents.extend(live);
        let pins = Pins {
            record: pins.record,
            intents,
        };
        events.push(pins.list_event(conn)?);
    }
    Ok(events)
}

/// The rebuilt core.db (`maintenance::rebuild_events`) has none of our queued changes and the
/// merges leave a field that an intent holds unstated (they rely on the core already showing
/// ours): this tells the new core the flags every live intent holds, with the features of a chat
/// the core has not got yet in front, and moves the told bases to them. The pin list needs no
/// line here: the account record is merged again by the full sync and states the whole list with
/// the live pin intents applied. Call it after the rebuild cleared the told bases and BEFORE
/// `storage::reset` empties the stored records.
pub(crate) fn restate_live(
    conn: &Connection,
    account: &Account,
    now: i64,
) -> Result<Vec<BackendEvent>> {
    let live = all_intents(conn)?;
    let values: Vec<(&str, &str, Option<i64>)> = live
        .iter()
        .map(|i| (i.target.as_str(), i.field.as_str(), i.value))
        .collect();
    let events = state_values(conn, account, &values, now)?;
    features::with_features(conn, account, events, now)
}

// ---------------------------------------------------------------------------------------------
// The three-way rule in the merges (§3.3)
// ---------------------------------------------------------------------------------------------

/// What a record the phone wrote does to one live intent of its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// `theirs == base`: the phone did not touch the field. The intent STAYS; the merge states
    /// nothing for the field (the core already shows ours) and the told base keeps OURS.
    Stays,
    /// `theirs != base` and `theirs == ours`: done elsewhere. The intent is deleted; there is
    /// nothing to state.
    DoneElsewhere,
    /// `theirs != base` and `theirs != ours`: THE PHONE WINS (Signal's own rule for these
    /// fields). The intent is deleted, the merge states `theirs` to the core and the told base
    /// moves to it.
    PhoneWins,
}

/// THE three-way rule, the one place that decides (pure): `ours` is the intent's `value`, `base`
/// the newest stored record's value when the intent was made (moved to the written value after a
/// landing, H3), `theirs` the value of the record being merged. The intent's `shown` (what the
/// core showed before the first click: the click-back test of L6) is not read: the table has no
/// row for it. Our own written records reach it as "theirs" only after a landing moved the base
/// to the written value (`theirs == base`, the intent stays): the 200 path stores them before any
/// sync ([`land_written`]), and a landing found after a restart runs in the first merge zone of
/// the manifest that lists them ([`resolve_in_flight`]).
pub(crate) fn three_way(intent: &Intent, theirs: Option<i64>) -> Verdict {
    if theirs == intent.base {
        Verdict::Stays
    } else if theirs == intent.value {
        Verdict::DoneElsewhere
    } else {
        Verdict::PhoneWins
    }
}

/// What a merge does with one field of a chat after the rule ran (or did not, for lack of an
/// intent).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Settled {
    /// No intent for the field: the merge works as it always did.
    #[default]
    Open,
    /// Our intent stays, or the phone did the same: the merge states nothing for the field and
    /// the mirror (A1's told base, the `groups` columns) keeps OURS (`Some(value)`; the value of
    /// a mute is its end, `None` unmuted).
    Held(Option<i64>),
    /// The phone wins: the merge states the record's value whether or not it differs from the
    /// mirror, and the mirror takes it.
    Theirs,
}

impl Settled {
    /// Whether the merge states the field: an open field when the normal diff says it changed,
    /// a phone win always, a held field never.
    pub(crate) fn states(self, differs: bool) -> bool {
        match self {
            Self::Open => differs,
            Self::Held(_) => false,
            Self::Theirs => true,
        }
    }

    /// The flag the mirror keeps: ours when held, else the record's.
    pub(crate) fn flag(self, theirs: bool) -> bool {
        match self {
            Self::Held(ours) => ours == Some(1),
            Self::Open | Self::Theirs => theirs,
        }
    }

    /// The mute the mirror keeps: ours when held, else the record's.
    pub(crate) fn mute(self, theirs: Option<i64>) -> Option<i64> {
        match self {
            Self::Held(ours) => ours,
            Self::Open | Self::Theirs => theirs,
        }
    }
}

/// What the rule made of the three flags of one chat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Settlement {
    pub(crate) archived: Settled,
    pub(crate) marked_unread: Settled,
    pub(crate) muted_until: Settled,
}

impl Settlement {
    /// OUR own record's settlement (review N6, L1): a field no intent holds ([`Settled::Open`]) is
    /// held at `told`, what the core was last told: the merge states nothing for it and the
    /// mirror keeps it. A field an intent holds keeps the rule's verdict.
    pub(crate) fn quiet(self, told: Flags) -> Self {
        let hold = |settled: Settled, value: Option<i64>| match settled {
            Settled::Open => Settled::Held(value),
            other => other,
        };
        Self {
            archived: hold(self.archived, flag_value(told.archived)),
            marked_unread: hold(self.marked_unread, flag_value(told.marked_unread)),
            muted_until: hold(self.muted_until, told.muted_until),
        }
    }
}

/// Whether any intent is stored: a merge zone asks once, and with none it runs no rule.
pub(crate) fn any_intent(conn: &Connection) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT EXISTS (SELECT 1 FROM storage_intents)")?
        .query_row([], |r| r.get(0))?)
}

/// The live intents of one target, oldest `seq` first.
fn intents_of(conn: &Connection, target: &str) -> Result<Vec<Intent>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {INTENT_COLUMNS} FROM storage_intents WHERE target = ?1 ORDER BY seq"
    ))?;
    let rows = stmt
        .query_map([target], intent_of)?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Settles `intent` against the value `theirs` of the record being merged: the rule, and the
/// deletion of what the rule closed.
fn settle(conn: &Connection, intent: &Intent, theirs: Option<i64>) -> Result<Settled> {
    Ok(match three_way(intent, theirs) {
        Verdict::Stays => Settled::Held(intent.value),
        Verdict::DoneElsewhere => {
            delete_intent(conn, &intent.target, &intent.field)?;
            Settled::Held(intent.value)
        }
        Verdict::PhoneWins => {
            delete_intent(conn, &intent.target, &intent.field)?;
            Settled::Theirs
        }
    })
}

/// The merge of a contact, group or account record: runs the rule for the archive, mark and mute
/// intents of `target` against the record's flags. A target with no intent costs one read and
/// settles nothing. (Pins and the safety number have their own functions below: they are not
/// flags of the chat.)
pub(crate) fn settle_flags(conn: &Connection, target: &str, theirs: Flags) -> Result<Settlement> {
    let mut settlement = Settlement::default();
    for intent in intents_of(conn, target)? {
        let (slot, value) = match intent.field.as_str() {
            ARCHIVED => (&mut settlement.archived, flag_value(theirs.archived)),
            MARKED_UNREAD => (
                &mut settlement.marked_unread,
                flag_value(theirs.marked_unread),
            ),
            MUTED_UNTIL => (&mut settlement.muted_until, theirs.muted_until),
            _ => continue,
        };
        *slot = settle(conn, &intent, value)?;
    }
    Ok(settlement)
}

/// The merge of an account record: runs the rule for every pin intent (`theirs` = whether the
/// record lists the chat) and returns the record as the core must see its pin list: the pin
/// intents that STAY applied to the record's entries (our removals dropped, our additions last
/// in `seq` order, every other entry as it is). `None` when no pin intent stays: the record
/// itself is the list. The record is not changed.
pub(crate) fn settle_pins(
    conn: &Connection,
    record: &proto::AccountRecord,
) -> Result<Option<proto::AccountRecord>> {
    let mut staying = Vec::new();
    for intent in intents_of(conn, ACCOUNT_TARGET)? {
        let Some(chat) = intent.field.strip_prefix(PIN_PREFIX) else {
            continue;
        };
        let listed = record
            .pinned_conversations
            .iter()
            .any(|entry| entry_target(entry).as_deref() == Some(chat));
        match three_way(&intent, flag_value(listed)) {
            Verdict::Stays => staying.push(intent),
            Verdict::DoneElsewhere | Verdict::PhoneWins => {
                delete_intent(conn, &intent.target, &intent.field)?;
            }
        }
    }
    Ok(with_pins_applied(record, staying))
}

/// `record` with `intents` (pin intents, oldest first) applied to its pin list; `None` when there
/// are none.
fn with_pins_applied(
    record: &proto::AccountRecord,
    intents: Vec<Intent>,
) -> Option<proto::AccountRecord> {
    if intents.is_empty() {
        return None;
    }
    let pins = Pins {
        record: record.clone(),
        intents,
    };
    let mut effective = record.clone();
    effective.pinned_conversations = pins.effective_entries();
    Some(effective)
}

/// The stored account record as the core must see its pin list when the list is restated for a
/// reason that is not a merge (`storage::pins_with_new_group`): EVERY live pin intent applied, so
/// the restatement does not undo a pin or an unpin that has not reached the phone. `None` when no
/// pin intent is live: the record itself is the list.
pub(crate) fn with_live_pins(
    conn: &Connection,
    record: &proto::AccountRecord,
) -> Result<Option<proto::AccountRecord>> {
    Ok(with_pins_applied(record, pin_intents(conn)?))
}

/// The merge of a contact record that carries a safety number's key and state: runs the rule
/// against a live identity intent of the SAME key (an intent for another key is deleted by
/// [`identity_for`] and nothing is stated). `None` when there is no intent for this key; the
/// intent is deleted unless it stays. The caller applies the record's state unless the verdict
/// is [`Verdict::Stays`] (the click here is newer than the phone's record).
pub(crate) fn settle_identity(
    conn: &Connection,
    target: &str,
    record_key: &[u8],
    record_state: i32,
) -> Result<Option<Verdict>> {
    let Some(intent) = identity_for(conn, target, record_key)? else {
        return Ok(None);
    };
    let verdict = three_way(&intent, Some(i64::from(record_state)));
    if verdict != Verdict::Stays {
        delete_intent(conn, target, IDENTITY)?;
    }
    Ok(Some(verdict))
}

#[cfg(test)]
mod tests;
