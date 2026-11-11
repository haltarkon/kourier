//! The rows of the Threema `backend.db`: this device's identity and the two permanent nonce stores
//! (schema 1), what a device join imports (schema 2, TH4), the envelopes the receive path defers for a
//! later build (schema 3, TH5: `deferred` with its bounds and the kept sum of `deferred_total`, the
//! re-map's pages). Plain functions over a connection; every write takes a [`Tx`], so it happens
//! inside a zone ([`kourier_db::ZoneDb::zone`]). What is being sent (schema 4, TH6a) is `send::queue`'s;
//! its items are re-exported here as the crate's test doors.
//!
//! `deferred` holds PLAINTEXT envelopes (an edit's text, a file's key) for up to 45 days (critic L8):
//! inside this SQLCipher file only, never in a log. The user's Clear Chat or Delete Chat on THIS device
//! deletes the rows of that chat (TH5b R1, [`clear_deferred`]); a clear on the phone is not synced (D2D
//! has no such message), so its rows stay until their age.
//!
//! THE JOIN'S TWO ZONES (TH4 decision H1): ZONE 1 [`import_join`] writes everything the phone handed
//! over (the identity with slot `new`, the profile, the settings, the contacts, groups, lists, held
//! blobs and hashed nonces) and NO `core_outbox` row; ZONE 2 [`ready_join`], after the mediator's first
//! `ServerInfo` said the slot is NEW, writes the `core_outbox` rows of `join::events` over
//! `load_snapshot` (`AccountReady` first) AND the marker row of `join_ready` (S4: [`is_join_ready`]).
//! `Rebuild` writes the same rows through the same zone 2, but only over a file that carries the marker
//! (the actor's start rule): an identity without it is a join that stopped between its zones.
//!
//! Public for the crate's own integration tests only (`#[doc(hidden)]` in `lib.rs`); no libthreema
//! type appears in a public item here ([`Import`] is opaque outside the crate).

use std::collections::HashSet;
use std::fmt;
use std::rc::Rc;

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::{Connection, OptionalExtension, params};
use kourier_db::{Tx, ZoneDb};
use kourier_model::{CmdId, ConvKey, Redacted, UserId};
use libthreema::protobuf::d2d_sync;
use prost::Message;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::db::{self, Points};
use crate::error::{Result, StoreError, sql};
use crate::failpoints as fp;
use crate::join::events::{self, SnapGroup, Snapshot};
use crate::outbox;
use crate::reflect::classify::{EXEMPT_KINDS, LOW_VALUE_KINDS};

pub use crate::db::open;
pub use crate::error::SendRefused;
pub use crate::join::essential::Import;
pub use crate::leader::zone::{
    Discarded, Inbound, InboundAck, InboundRow, InboundState, InboundSwept, Kept, RECEIPT_IDS,
    RULE_PENDING_EXPIRED, Received, Receiving, fail_inbound_zone, inbound_row, inbound_rows,
    mark_inbound_reflected, receive_inbound, sweep_inbound,
};
pub use crate::leader::{BodyOf, OpenedInbound, open_inbound, write_body};
pub use crate::reflect::remap::Remapped;
pub use crate::reflect::{fail_deferred_of_length, fail_deferred_of_lengths, reflected_zones};
pub use crate::send::prepare::{
    NewBody, NewMessage, Specific, To, prepare_outgoing, seal_outgoing, seal_sent_update,
};
pub use crate::send::queue::{
    EVICT_AFTER_MS, JOB_BUDGET, NewFate, NewOrigin, NewReceiver, RULE_JOB_EXPIRED, RULE_QUEUE_FULL,
    RULE_TRANSMIT_EVICTED, RULE_TRANSMIT_EXPIRED, ReceiverState, Runnable, SealedRows,
    SealedUpdate, SendBudget, SendEnd, SendOrigin, SendReceiver, SendState, SendTask, Swept,
    TRANSMIT_BUDGET, count_failed_run, fail_send_zone, give_up_send_job, mark_send_acks,
    mark_send_done, mark_send_reflected, next_send, prepare_send, prepare_send_zone, send_ledger,
    send_receivers, send_task, send_tasks, sweep_sends,
};
pub use crate::send::{NewJob, enqueue_job};

/// Whether the device took a new slot on the mediator or an existing one (D2M `DeviceSlotState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// The device's slot did not exist before its first login.
    New,
    /// The slot exists (every login after the first).
    Existing,
}

impl SlotState {
    fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Existing => "existing",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "new" => Some(Self::New),
            "existing" => Some(Self::Existing),
            _ => None,
        }
    }
}

/// This device of the Threema ID: the one `identity` row. `Debug` prints no key, no ID, no device
/// id and no label.
#[derive(Clone, PartialEq, Eq)]
pub struct Identity {
    /// The Threema ID (8 characters; personal data).
    pub threema_id: String,
    /// CK, the client key (secret).
    pub client_key: Zeroizing<[u8; 32]>,
    /// The chat server group: one or more of `[A-Za-z0-9-]` (the schema's CHECK).
    pub server_group: String,
    /// DGK, the device group key (secret).
    pub device_group_key: Zeroizing<[u8; 32]>,
    /// The device cookie (secret).
    pub device_cookie: Zeroizing<[u8; 16]>,
    /// `D2xDeviceId`.
    pub d2m_device_id: u64,
    /// `CspDeviceId`.
    pub csp_device_id: u64,
    /// The mediator slot this device expects.
    pub slot_state: SlotState,
    /// The device's label on the mediator ("Kourier (<host>)").
    pub device_label: String,
    /// When the device was linked (ms since the epoch).
    pub linked_at_ms: i64,
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("slot_state", &self.slot_state)
            .field("linked_at_ms", &self.linked_at_ms)
            .finish_non_exhaustive()
    }
}

/// The u64 ids are stored as the i64 of the same 64 bits (SQLite has no unsigned integer).
pub(crate) fn to_sql_u64(v: u64) -> i64 {
    i64::from_ne_bytes(v.to_ne_bytes())
}

pub(crate) fn from_sql_u64(v: i64) -> u64 {
    u64::from_ne_bytes(v.to_ne_bytes())
}

/// Whether the identity row exists (a linked device).
pub fn identity_exists(conn: &Connection) -> Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM identity)", [], |r| r.get(0))
        .map_err(sql("look up identity failed"))
}

/// Writes the identity row. There is ONE: a second save fails (a linked device is never
/// overwritten; a new link is a new account with a new file).
pub fn save_identity(tx: &Tx<'_>, identity: &Identity) -> Result<()> {
    tx.execute(
        "INSERT INTO identity (id, threema_id, client_key, server_group, device_group_key,
           device_cookie, d2m_device_id, csp_device_id, slot_state, device_label, linked_at_ms)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            identity.threema_id,
            &identity.client_key[..],
            identity.server_group,
            &identity.device_group_key[..],
            &identity.device_cookie[..],
            to_sql_u64(identity.d2m_device_id),
            to_sql_u64(identity.csp_device_id),
            identity.slot_state.as_str(),
            identity.device_label,
            identity.linked_at_ms,
        ],
    )
    .map_err(sql("save identity failed"))?;
    Ok(())
}

/// The mediator answered this device's login with `ServerInfo`: its slot exists from now on (brief
/// §2.4). ONE row, so one plain zone (no failpoint pair: the zone rule is for writes of more than one
/// row). A file without the identity row is an error.
pub fn set_slot_existing(db: &ZoneDb) -> Result<()> {
    let changed = db.zone(|tx| {
        tx.execute(
            "UPDATE identity SET slot_state = ?1 WHERE id = 1",
            params![SlotState::Existing.as_str()],
        )
        .map_err(sql("save the slot state failed"))
    })?;
    if changed != 1 {
        return Err(StoreError::Failed("save the slot state failed"));
    }
    Ok(())
}

/// The fixed-size secret in a BLOB column, or the row is unreadable.
fn secret<const N: usize>(bytes: Vec<u8>) -> Result<Zeroizing<[u8; N]>> {
    let bytes = Zeroizing::new(bytes);
    let mut out = Zeroizing::new([0u8; N]);
    if bytes.len() != N {
        return Err(StoreError::Unreadable("identity row unreadable"));
    }
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// The identity row, or `None` before the device is linked.
pub fn load_identity(conn: &Connection) -> Result<Option<Identity>> {
    type Raw = (
        String,
        Vec<u8>,
        String,
        Vec<u8>,
        Vec<u8>,
        i64,
        i64,
        String,
        String,
        i64,
    );
    let raw: Option<Raw> = conn
        .query_row(
            "SELECT threema_id, client_key, server_group, device_group_key, device_cookie,
                    d2m_device_id, csp_device_id, slot_state, device_label, linked_at_ms
             FROM identity WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        )
        .optional()
        .map_err(sql("load identity failed"))?;
    let Some((id, ck, group, dgk, cookie, d2m, csp, slot, label, linked)) = raw else {
        return Ok(None);
    };
    Ok(Some(Identity {
        threema_id: id,
        client_key: secret(ck)?,
        server_group: group,
        device_group_key: secret(dgk)?,
        device_cookie: secret(cookie)?,
        d2m_device_id: from_sql_u64(d2m),
        csp_device_id: from_sql_u64(csp),
        slot_state: SlotState::parse(&slot)
            .ok_or(StoreError::Unreadable("identity row unreadable"))?,
        device_label: label,
        linked_at_ms: linked,
    }))
}

/// Which permanent nonce store: the chat server's (CSP) or the device-to-device one (D2D).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonceScope {
    /// Nonces of messages to and from the chat server.
    Csp,
    /// Nonces of reflected (device-to-device) messages.
    D2d,
}

impl NonceScope {
    fn table(self) -> &'static str {
        match self {
            Self::Csp => "csp_nonce",
            Self::D2d => "d2d_nonce",
        }
    }
}

/// Adds hashed nonces (HMAC-SHA256 with the identity as key) to the store of `scope`. A nonce
/// already there is no error and stays one row.
pub fn add_nonces(tx: &Tx<'_>, scope: NonceScope, hashes: &[[u8; 32]]) -> Result<()> {
    let insert = format!("INSERT OR IGNORE INTO {} (hash) VALUES (?1)", scope.table());
    let mut stmt = tx
        .prepare_cached(&insert)
        .map_err(sql("add nonces failed"))?;
    for hash in hashes {
        stmt.execute(params![&hash[..]])
            .map_err(sql("add nonces failed"))?;
    }
    Ok(())
}

/// Whether the store of `scope` holds the hashed nonce.
pub fn contains_nonce(conn: &Connection, scope: NonceScope, hash: &[u8; 32]) -> Result<bool> {
    let query = format!(
        "SELECT EXISTS(SELECT 1 FROM {} WHERE hash = ?1)",
        scope.table()
    );
    conn.query_row(&query, params![&hash[..]], |r| r.get(0))
        .map_err(sql("look up nonce failed"))
}

/// Whether the file holds the contact `identity` (a reflected message's sender, or the contact an own
/// message went to: decision M2 drops a message of a contact the file does not know).
pub(crate) fn contact_exists(conn: &Connection, identity: &str) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM contact WHERE identity = ?1)",
        params![identity],
        |r| r.get(0),
    )
    .map_err(sql("look up a contact failed"))
}

/// Whether the file holds the group (`creator`, `group_id`) (decision M2: a group message of a group the
/// file does not know is dropped).
pub(crate) fn group_exists(conn: &Connection, creator: &str, group_id: u64) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM grp WHERE creator = ?1 AND group_id = ?2)",
        params![creator, to_sql_u64(group_id)],
        |r| r.get(0),
    )
    .map_err(sql("look up a group failed"))
}

/// The stored `d2d_sync.Contact` of `identity` (TH5 A7a: the base a `ContactSync` delta applies to).
pub(crate) fn load_contact(conn: &Connection, identity: &str) -> Result<Option<d2d_sync::Contact>> {
    let bytes: Option<Vec<u8>> = conn
        .query_row(
            "SELECT sync FROM contact WHERE identity = ?1",
            params![identity],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql("load a contact failed"))?;
    bytes.map(|b| decode(&b)).transpose()
}

/// The stored `d2d_sync.Group` of (`creator`, `group_id`) (the base a `GroupSync` delta applies to).
pub(crate) fn load_group(
    conn: &Connection,
    creator: &str,
    group_id: u64,
) -> Result<Option<d2d_sync::Group>> {
    let bytes: Option<Vec<u8>> = conn
        .query_row(
            "SELECT sync FROM grp WHERE creator = ?1 AND group_id = ?2",
            params![creator, to_sql_u64(group_id)],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql("load a group failed"))?;
    bytes.map(|b| decode(&b)).transpose()
}

/// The Identity Blocked Steps' step 9 (TH6a), as a set: every identity that is the creator or a member of a stored
/// group the user is still in (`events::is_member`: a group the user left, or was kicked from, counts for nobody, its
/// creator and members with it). ONE walk of `grp`, a decode per row: the send steps take it once per PREPARE, lazily,
/// when the first receiver needs it (S-M2: the look per receiver was a walk of every group per member), and ask only
/// under `BLOCK_UNKNOWN`, for a contact that is not direct. A row that does not decode fails the whole look.
pub(crate) fn identities_in_groups(conn: &Connection) -> Result<HashSet<String>> {
    const OP: &str = "look through the groups failed";
    let mut stmt = conn
        .prepare_cached("SELECT creator, sync FROM grp")
        .map_err(sql(OP))?;
    let mut rows = stmt.query([]).map_err(sql(OP))?;
    let mut identities = HashSet::new();
    while let Some(row) = rows.next().map_err(sql(OP))? {
        let creator: String = row.get(0).map_err(sql(OP))?;
        let sync: Vec<u8> = row.get(1).map_err(sql(OP))?;
        let group: d2d_sync::Group = decode(&sync)?;
        if !events::is_member(&group) {
            continue;
        }
        identities.insert(creator);
        identities.extend(
            group
                .member_identities
                .into_iter()
                .flat_map(|members| members.identities),
        );
    }
    Ok(identities)
}

/// Whether the stored settings block `identity` (a file without a settings row blocks nobody).
pub(crate) fn is_blocked(conn: &Connection, identity: &str) -> Result<bool> {
    Ok(load_settings(conn)?
        .and_then(|s| s.blocked_identities)
        .is_some_and(|b| b.identities.iter().any(|i| i == identity)))
}

/// How many contacts and groups the file holds (the bounds of a sync that creates one).
pub(crate) fn sync_counts(conn: &Connection) -> Result<(u64, u64)> {
    conn.query_row(
        "SELECT (SELECT count(*) FROM contact), (SELECT count(*) FROM grp)",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )
    .map(|(c, g)| (c.unsigned_abs(), g.unsigned_abs()))
    .map_err(sql("count the contacts and groups failed"))
}

/// Writes a contact's WHOLE new `d2d_sync.Contact` (the prost encode of a delta's result, L10): a new
/// row has no conversation time (`last_update_at` NULL), an existing one keeps its time.
pub(crate) fn put_contact(tx: &Connection, sync: &d2d_sync::Contact) -> Result<()> {
    tx.execute(
        "INSERT INTO contact (identity, sync, last_update_at) VALUES (?1, ?2, NULL) \
         ON CONFLICT (identity) DO UPDATE SET sync = excluded.sync",
        params![sync.identity, sync.encode_to_vec()],
    )
    .map(|_| ())
    .map_err(sql("store a contact failed"))
}

/// Writes a group's WHOLE new `d2d_sync.Group`: a new row's conversation time is `created_ms` (the
/// reflection's time, as Desktop's `lastUpdate: reflectedAt`), an existing one keeps its time.
pub(crate) fn put_group(
    tx: &Connection,
    creator: &str,
    group_id: u64,
    sync: &d2d_sync::Group,
    created_ms: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO grp (creator, group_id, sync, last_update_at) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (creator, group_id) DO UPDATE SET sync = excluded.sync",
        params![
            creator,
            to_sql_u64(group_id),
            sync.encode_to_vec(),
            created_ms
        ],
    )
    .map(|_| ())
    .map_err(sql("store a group failed"))
}

/// Deletes a group's row (`GroupSync.Delete`).
pub(crate) fn delete_group(tx: &Connection, creator: &str, group_id: u64) -> Result<()> {
    tx.execute(
        "DELETE FROM grp WHERE creator = ?1 AND group_id = ?2",
        params![creator, to_sql_u64(group_id)],
    )
    .map(|_| ())
    .map_err(sql("delete a group failed"))
}

/// The stored `d2d_sync.Settings` (TH5b B4, R13: the base a `SettingsSync` delta applies to); `None` in a
/// file without the row (a TH1 file).
pub(crate) fn load_settings(conn: &Connection) -> Result<Option<d2d_sync::Settings>> {
    let bytes: Option<Vec<u8>> = conn
        .query_row("SELECT sync FROM settings WHERE id = 1", [], |r| r.get(0))
        .optional()
        .map_err(sql("load the settings failed"))?;
    bytes.map(|b| decode(&b)).transpose()
}

/// The stored `d2d_sync.UserProfile` (the base a `UserProfileSync` delta applies to); `None` in a file
/// without the row (a TH1 file).
pub(crate) fn load_profile(conn: &Connection) -> Result<Option<d2d_sync::UserProfile>> {
    let bytes: Option<Vec<u8>> = conn
        .query_row("SELECT sync FROM own_profile WHERE id = 1", [], |r| {
            r.get(0)
        })
        .optional()
        .map_err(sql("load the profile failed"))?;
    bytes.map(|b| decode(&b)).transpose()
}

/// The stored `d2d_sync.DistributionList` of `id` (the base a `DistributionListSync` delta applies to).
pub(crate) fn load_list(conn: &Connection, id: u64) -> Result<Option<d2d_sync::DistributionList>> {
    let bytes: Option<Vec<u8>> = conn
        .query_row(
            "SELECT sync FROM dist_list WHERE id = ?1",
            params![to_sql_u64(id)],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql("load a list failed"))?;
    bytes.map(|b| decode(&b)).transpose()
}

/// How many distribution lists the file holds (the bound of a sync that creates one).
pub(crate) fn list_count(conn: &Connection) -> Result<u64> {
    conn.query_row("SELECT count(*) FROM dist_list", [], |r| r.get::<_, i64>(0))
        .map(i64::unsigned_abs)
        .map_err(sql("count the lists failed"))
}

/// Writes the WHOLE new `d2d_sync.Settings` (the one row, `id = 1`; created when the file had none).
pub(crate) fn put_settings(tx: &Connection, sync: &d2d_sync::Settings) -> Result<()> {
    tx.execute(
        "INSERT INTO settings (id, sync) VALUES (1, ?1) \
         ON CONFLICT (id) DO UPDATE SET sync = excluded.sync",
        params![sync.encode_to_vec()],
    )
    .map(|_| ())
    .map_err(sql("store the settings failed"))
}

/// Writes the WHOLE new `d2d_sync.UserProfile` (the one row, `id = 1`; created when the file had none).
pub(crate) fn put_profile(tx: &Connection, sync: &d2d_sync::UserProfile) -> Result<()> {
    tx.execute(
        "INSERT INTO own_profile (id, sync) VALUES (1, ?1) \
         ON CONFLICT (id) DO UPDATE SET sync = excluded.sync",
        params![sync.encode_to_vec()],
    )
    .map(|_| ())
    .map_err(sql("store the profile failed"))
}

/// Writes a list's WHOLE new `d2d_sync.DistributionList` under its id: a new row's conversation time is
/// `created_ms` (the reflection's time, as [`put_group`]), an existing one keeps its time.
pub(crate) fn put_list(
    tx: &Connection,
    sync: &d2d_sync::DistributionList,
    created_ms: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO dist_list (id, sync, last_update_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT (id) DO UPDATE SET sync = excluded.sync",
        params![
            to_sql_u64(sync.distribution_list_id),
            sync.encode_to_vec(),
            created_ms
        ],
    )
    .map(|_| ())
    .map_err(sql("store a list failed"))
}

/// Deletes a list's row (`DistributionListSync.Delete`).
pub(crate) fn delete_list(tx: &Connection, id: u64) -> Result<()> {
    tx.execute(
        "DELETE FROM dist_list WHERE id = ?1",
        params![to_sql_u64(id)],
    )
    .map(|_| ())
    .map_err(sql("delete a list failed"))
}

/// The core's word for a linked device: `AccountReady` with the Threema ID as the own id (L10
/// option B: no device id is reported to the contract before P5) and the profile's nickname as the
/// display name (`None`: none, or empty).
pub(crate) fn account_ready(threema_id: &str, nickname: Option<&str>) -> Result<BackendEvent> {
    let own = UserId::from_parts("threema", threema_id)
        .map_err(|_| StoreError::Failed("state the identity failed"))?;
    let nickname = nickname.map(str::trim).filter(|n| !n.is_empty());
    Ok(BackendEvent::AccountReady {
        own_ids: vec![own],
        device: None,
        display_name: nickname.map(|n| Redacted::new(n.to_owned())),
        number: None,
    })
}

const LINK_POINTS: Points = Points {
    inside: fp::INSIDE_SAVE_IDENTITY,
    after: fp::AFTER_SAVE_IDENTITY,
};

const NONCE_POINTS: Points = Points {
    inside: fp::INSIDE_ADD_NONCES,
    after: fp::AFTER_ADD_NONCES,
};

/// The link's commit (TH4's join in miniature): the identity row, the zone-2 marker AND
/// `AccountReady` in `core_outbox`, in ONE zone, so a device is never stored without the core being
/// told, nor told without being stored. The forwarder is woken after the commit. A second link fails
/// (one row).
pub fn record_link(db: &ZoneDb, identity: &Identity, now_ms: i64, wake: &Rc<Notify>) -> Result<()> {
    let ready = account_ready(&identity.threema_id, None)?;
    db::zone(db, LINK_POINTS, |tx| {
        save_identity(tx, identity)?;
        mark_join_ready(tx, now_ms)?;
        outbox::append_events(tx, std::slice::from_ref(&ready), now_ms, wake).map(drop)
    })
}

/// Writes the zone-2 marker (S4): the one `join_ready` row, `at` = `now_ms`; an existing row stays (the
/// first time is kept). Zone 2 and [`record_link`] write it in their own zone; a test that seeds a
/// linked file with [`save_identity`] alone writes it in the same zone, as the join would have. A file joined by this
/// build has told the core nothing older, so [`state_features_again`] has nothing to say to it.
pub fn mark_join_ready(tx: &Tx<'_>, now_ms: i64) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO join_ready (id, at) VALUES (1, ?1)",
        params![now_ms],
    )
    .map_err(sql("mark the join ready failed"))?;
    tx.execute(
        "INSERT OR IGNORE INTO features_stated (id, version) VALUES (1, ?1)",
        params![FEATURES_VERSION],
    )
    .map_err(sql("mark the join ready failed"))?;
    Ok(())
}

/// Whether zone 2 of the join (or the link's commit) wrote the marker: the file's identity is a
/// finished link the core may be told of. An identity without it stopped between the join's zones.
pub fn is_join_ready(conn: &Connection) -> Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM join_ready)", [], |r| r.get(0))
        .map_err(sql("look up the join marker failed"))
}

/// A batch of hashed nonces of BOTH stores in ONE zone (a reflected message and the chat server's
/// message it carries are taken together, or neither is).
pub fn add_nonce_batches(db: &ZoneDb, csp: &[[u8; 32]], d2d: &[[u8; 32]]) -> Result<()> {
    db::zone(db, NONCE_POINTS, |tx| {
        add_nonces(tx, NonceScope::Csp, csp)?;
        add_nonces(tx, NonceScope::D2d, d2d)
    })
}

const IMPORT_POINTS: Points = Points {
    inside: fp::INSIDE_IMPORT,
    after: fp::AFTER_IMPORT,
};

const READY_POINTS: Points = Points {
    inside: fp::INSIDE_READY,
    after: fp::AFTER_READY,
};

/// A `u64` time (ms) as SQLite's INTEGER; `essential` refused every time past `i64::MAX` (E4), so a
/// failure here is a bug, not data.
fn time(ms: u64) -> Result<i64> {
    i64::try_from(ms).map_err(|_| StoreError::Failed("import the join failed"))
}

/// ZONE 1 OF A JOIN (decision H1): everything of `import` in ONE zone (failpoints `INSIDE_IMPORT` /
/// `AFTER_IMPORT`), in this order: the identity row (slot `new`, `device_label`, linked at `now_ms`),
/// the profile, the settings, every contact, group and distribution list (each the protobuf bytes of
/// its `d2d_sync.*` message, the keys as columns, the u64 ids as the i64 of the same bits), the held
/// blobs (received at `now_ms`), the hashed nonces of both stores (`INSERT OR IGNORE`). NO
/// `core_outbox` row: the core hears of the account only in zone 2 ([`ready_join`]). A file that has an
/// identity already fails (one row) and keeps what it had. The keys stay in `Zeroizing`; nothing is
/// logged but the operation's name.
pub fn import_join(db: &ZoneDb, import: &Import, device_label: &str, now_ms: i64) -> Result<()> {
    const OP: &str = "import the join failed";
    let identity = import.identity(device_label.to_owned(), now_ms);
    db::zone(db, IMPORT_POINTS, |tx| {
        save_identity(tx, &identity)?;
        tx.execute(
            "INSERT INTO own_profile (id, sync) VALUES (1, ?1)",
            params![import.profile.encode_to_vec()],
        )
        .map_err(sql(OP))?;
        tx.execute(
            "INSERT INTO settings (id, sync) VALUES (1, ?1)",
            params![import.settings.encode_to_vec()],
        )
        .map_err(sql(OP))?;
        let mut contact = tx
            .prepare_cached(
                "INSERT INTO contact (identity, sync, last_update_at) VALUES (?1, ?2, ?3)",
            )
            .map_err(sql(OP))?;
        for c in &import.contacts {
            let at = c.last_update_at.map(time).transpose()?;
            contact
                .execute(params![c.sync.identity, c.sync.encode_to_vec(), at])
                .map_err(sql(OP))?;
        }
        drop(contact);
        let mut group = tx
            .prepare_cached(
                "INSERT INTO grp (creator, group_id, sync, last_update_at) VALUES (?1, ?2, ?3, ?4)",
            )
            .map_err(sql(OP))?;
        for g in &import.groups {
            group
                .execute(params![
                    g.creator,
                    to_sql_u64(g.group_id),
                    g.sync.encode_to_vec(),
                    time(g.last_update_at)?
                ])
                .map_err(sql(OP))?;
        }
        drop(group);
        let mut list = tx
            .prepare_cached("INSERT INTO dist_list (id, sync, last_update_at) VALUES (?1, ?2, ?3)")
            .map_err(sql(OP))?;
        for l in &import.lists {
            list.execute(params![
                to_sql_u64(l.id),
                l.sync.encode_to_vec(),
                time(l.last_update_at)?
            ])
            .map_err(sql(OP))?;
        }
        drop(list);
        let mut blob = tx
            .prepare_cached("INSERT INTO held_blob (id, data, received_at) VALUES (?1, ?2, ?3)")
            .map_err(sql(OP))?;
        for b in &import.blobs {
            blob.execute(params![b.id, b.data, now_ms])
                .map_err(sql(OP))?;
        }
        drop(blob);
        add_nonces(tx, NonceScope::Csp, &import.csp_nonces)?;
        add_nonces(tx, NonceScope::D2d, &import.d2d_nonces)
    })
}

/// One stored `d2d_sync.*` message, or the row is unreadable.
fn decode<M: Message + Default>(bytes: &[u8]) -> Result<M> {
    M::decode(bytes).map_err(|_| StoreError::Unreadable("a joined row unreadable"))
}

/// The rows of `query` (one BLOB column, then `extra` read by `f`), decoded.
fn blobs<T>(
    conn: &Connection,
    query: &str,
    mut f: impl FnMut(&kourier_db::rusqlite::Row<'_>, Vec<u8>) -> Result<T>,
) -> Result<Vec<T>> {
    const OP: &str = "load the joined account failed";
    let mut stmt = conn.prepare(query).map_err(sql(OP))?;
    let mut rows = stmt.query([]).map_err(sql(OP))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(sql(OP))? {
        let bytes: Vec<u8> = row.get(0).map_err(sql(OP))?;
        out.push(f(row, bytes)?);
    }
    Ok(out)
}

/// What zone 1 wrote, read back (the protobuf BLOBs decoded): the snapshot `join::events` states.
/// A file without the identity row fails; one without profile or settings rows (a TH1 file) has
/// `None` there.
pub(crate) fn load_snapshot(conn: &Connection) -> Result<Snapshot> {
    const OP: &str = "load the joined account failed";
    let own_id: Option<String> = conn
        .query_row("SELECT threema_id FROM identity WHERE id = 1", [], |r| {
            r.get(0)
        })
        .optional()
        .map_err(sql(OP))?;
    let own_id = own_id.ok_or(StoreError::Failed("the account has no identity"))?;
    let one = |query: &str| -> Result<Option<Vec<u8>>> {
        conn.query_row(query, [], |r| r.get(0))
            .optional()
            .map_err(sql(OP))
    };
    let profile = one("SELECT sync FROM own_profile WHERE id = 1")?
        .map(|b| decode::<d2d_sync::UserProfile>(&b))
        .transpose()?;
    let settings = one("SELECT sync FROM settings WHERE id = 1")?
        .map(|b| decode::<d2d_sync::Settings>(&b))
        .transpose()?;
    let contacts = blobs(
        conn,
        "SELECT sync FROM contact ORDER BY identity",
        |_, b| decode::<d2d_sync::Contact>(&b),
    )?;
    let groups = blobs(
        conn,
        "SELECT sync, creator, group_id FROM grp ORDER BY creator, group_id",
        |row, b| {
            Ok(SnapGroup {
                creator: row.get(1).map_err(sql(OP))?,
                group_id: from_sql_u64(row.get(2).map_err(sql(OP))?),
                sync: decode::<d2d_sync::Group>(&b)?,
            })
        },
    )?;
    let lists = blobs(conn, "SELECT sync FROM dist_list ORDER BY id", |_, b| {
        decode::<d2d_sync::DistributionList>(&b)
    })?;
    Ok(Snapshot {
        own_id,
        profile,
        settings,
        contacts,
        groups,
        lists,
    })
}

/// The rows of `snapshot` (`join::events`), or the operation fails by name.
fn rows_of(snapshot: &Snapshot) -> Result<Vec<Vec<BackendEvent>>> {
    events::events(snapshot)
        .map_err(|events::BadId| StoreError::Failed("state the joined account failed"))
}

/// The `core_outbox` seqs zone 2 wrote: row 1 (`AccountReady` first; the core owns the account once it
/// applied it) and the last row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ready {
    /// The seq of row 1.
    pub first: i64,
    /// The seq of the last row (equal to `first` for an account with nothing else to state).
    pub last: i64,
}

/// ZONE 2 OF A JOIN (decision H1), and the start of a `Rebuild`: the `core_outbox` rows of
/// `join::events` over [`load_snapshot`] (one row each, `AccountReady` first) and the marker row
/// ([`mark_join_ready`], S4) in ONE zone (failpoints `INSIDE_READY` / `AFTER_READY`), created at `now_ms`;
/// the forwarder is woken after the commit. Returns the seqs of the first and the last row.
pub fn ready_join(db: &ZoneDb, now_ms: i64, wake: &Rc<Notify>) -> Result<Ready> {
    db::zone(db, READY_POINTS, |tx| {
        let rows = rows_of(&load_snapshot(tx)?)?;
        let mut seqs = Vec::with_capacity(rows.len());
        for row in &rows {
            seqs.push(outbox::append_events(tx, row, now_ms, wake)?);
        }
        mark_join_ready(tx, now_ms)?;
        match (seqs.first(), seqs.last()) {
            (Some(first), Some(last)) => Ok(Ready {
                first: *first,
                last: *last,
            }),
            _ => Err(StoreError::Failed("state the joined account failed")),
        }
    })
}

/// What this build's chats can do differs from what the older builds told the core; raised with [`events::features`].
const FEATURES_VERSION: i64 = 1;

/// One zone at the first start of a build whose [`events::features`] changed: every chat the file shows says again what it
/// can do. A hidden contact's chat is left out, as at a join: its next message carries the features.
pub fn state_features_again(db: &ZoneDb, now_ms: i64, wake: &Rc<Notify>) -> Result<()> {
    const OP: &str = "state the chats' features failed";
    db.zone(|tx: &mut Tx<'_>| {
        let stated: Option<i64> = tx
            .query_row("SELECT version FROM features_stated", [], |row| row.get(0))
            .optional()
            .map_err(sql(OP))?;
        if stated.is_some_and(|version| version >= FEATURES_VERSION) {
            return Ok(());
        }
        let snapshot = load_snapshot(tx)?;
        let mut patches = Vec::new();
        for contact in &snapshot.contacts {
            if let Ok(Some(patch)) = events::direct_patch(contact) {
                patches.push(BackendEvent::ConversationUpsert(patch));
            }
        }
        for group in &snapshot.groups {
            if let Ok(Some(patch)) = events::group_patch(group, &snapshot.own_id) {
                patches.push(BackendEvent::ConversationUpsert(patch));
            }
        }
        for row in patches.chunks(64) {
            outbox::append_events(tx, row, now_ms, wake)?;
        }
        tx.execute(
            "INSERT INTO features_stated(id, version) VALUES (1, ?1) \
             ON CONFLICT (id) DO UPDATE SET version = excluded.version",
            params![FEATURES_VERSION],
        )
        .map_err(sql(OP))?;
        Ok(())
    })
}

/// What a `Rebuild` (and zone 2) states over this file: `join::events` over [`load_snapshot`]. For the
/// crate's integration tests.
#[doc(hidden)]
pub fn stated_events(conn: &Connection) -> Result<Vec<Vec<BackendEvent>>> {
    rows_of(&load_snapshot(conn)?)
}

/// What `import` states once stored, computed in memory without the store (the oracle of the store's
/// round trip: [`stated_events`] after [`import_join`] must equal it). For the crate's integration
/// tests.
#[doc(hidden)]
pub fn import_events(import: &Import) -> Result<Vec<Vec<BackendEvent>>> {
    rows_of(&Snapshot::of(import))
}

// DEFERRED, NOT LOST (schema 3, `migrations/0003_receive.sql`) -----------------------------------

/// The batch zone of the reflection loop (A5a): its nonces, its `deferred` rows and its `core_outbox`
/// rows in ONE zone, before any ack.
pub(crate) const REFLECTED_POINTS: Points = Points {
    inside: fp::INSIDE_REFLECTED,
    after: fp::AFTER_REFLECTED,
};

/// One zone of the start's re-map of `deferred` (A8): at most [`REMAP_PAGE`] rows, each deleted
/// ([`delete_deferred`]) or given its new version ([`set_deferred_version`]) with the `core_outbox` rows
/// of its events.
pub(crate) const REMAP_POINTS: Points = Points {
    inside: fp::INSIDE_REMAP,
    after: fp::AFTER_REMAP,
};

const EXPIRE_POINTS: Points = Points {
    inside: fp::INSIDE_EXPIRE,
    after: fp::AFTER_EXPIRE,
};

/// The zone of the user's Clear Chat / Delete Chat (TH5b R1, [`clear_deferred`]): the chat's `deferred` rows
/// deleted and the command's `CommandPersisted` written, together.
const CLEAR_POINTS: Points = Points {
    inside: fp::INSIDE_CLEAR,
    after: fp::AFTER_CLEAR,
};

/// The most rows one page of the re-map holds, and so one re-map zone (decision M3: the
/// architecture's stage-1 batch of 30).
pub const REMAP_PAGE: usize = 30;

/// The bounds of `deferred` (decision C14). [`DeferredBounds::REAL`] everywhere but in the tests of
/// the bounds, which pass small ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeferredBounds {
    /// The most rows; over, the oldest rows go.
    pub rows: i64,
    /// The most bytes of `envelope` in all (the kept sum); over, the oldest rows go.
    pub bytes: i64,
    /// A row whose `received_at` is MORE than this many ms before now goes.
    pub age_ms: i64,
}

impl DeferredBounds {
    /// 20,000 rows, 256 MiB, 45 days (C14; 45 days is the architecture's poison age).
    pub const REAL: Self = Self {
        rows: 20_000,
        bytes: 256 * 1024 * 1024,
        age_ms: 45 * 24 * 60 * 60 * 1000,
    };

    /// These bounds without the age bound (no row is ever old enough): the batch zones of a socket whose
    /// clock the mediator's `ServerInfo` did not prove (round B, S9: "Connect Anyway" through a skewed
    /// clock), so a clock set far ahead cannot empty `deferred`.
    #[must_use]
    pub const fn without_age(self) -> Self {
        Self {
            age_ms: i64::MAX,
            ..self
        }
    }
}

/// The rule names of the WARN lines the bounds write (one line per committed zone that dropped rows,
/// with the count; never a row's content).
pub const RULE_FULL: &str = "deferred-full";
/// See [`RULE_FULL`]: rows past the age bound.
pub const RULE_EXPIRED: &str = "deferred-expired";

/// Where a deferred envelope came from (the `source` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredSource {
    /// A D2M `Reflected` of another device (TH5).
    Reflected,
    /// The chat server, received as the leader (TH6).
    Csp,
}

impl DeferredSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reflected => "reflected",
            Self::Csp => "csp",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "reflected" => Some(Self::Reflected),
            "csp" => Some(Self::Csp),
            _ => None,
        }
    }
}

/// A row to defer. `envelope` is the plaintext AS RECEIVED (for `Reflected`: the decrypted
/// `d2d.Envelope` with its padding), 1 to 65,536 bytes (the schema's CHECK).
#[derive(Clone, Copy)]
pub struct NewDeferred<'a> {
    /// Where it came from.
    pub source: DeferredSource,
    /// The rule that deferred it: `[a-z0-9-]`, 1 to 32 characters (the schema's CHECK).
    pub kind: &'static str,
    /// The MAP_VERSION of this build; at least 1 (the schema's CHECK).
    pub map_version: u32,
    /// `Reflected.timestamp` (ms, the mediator's), or a CSP message's `created-at` as ms.
    pub server_at: u64,
    /// `Reflected.flags`, or a CSP message's flags.
    pub flags: u16,
    /// The plaintext.
    pub envelope: &'a [u8],
}

/// A stored deferred row. `Debug` prints neither the envelope nor its length.
#[derive(Clone, PartialEq, Eq)]
pub struct Deferred {
    /// Arrival order (a lower id is an older row; never reused); the re-map's page cursor.
    pub id: i64,
    /// Where it came from.
    pub source: DeferredSource,
    /// The rule that deferred it last.
    pub kind: String,
    /// The MAP_VERSION that deferred it last.
    pub map_version: u32,
    /// As [`NewDeferred::server_at`].
    pub server_at: u64,
    /// As [`NewDeferred::flags`].
    pub flags: u16,
    /// When its zone ran (ms, the local clock).
    pub received_at: i64,
    /// The plaintext (a message body: never in a log, an error or a test's output).
    pub envelope: Zeroizing<Vec<u8>>,
}

impl fmt::Debug for Deferred {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deferred")
            .field("id", &self.id)
            .field("source", &self.source)
            .field("kind", &self.kind)
            .field("map_version", &self.map_version)
            .finish_non_exhaustive()
    }
}

/// The rows the bounds deleted in one zone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dropped {
    /// The oldest rows, for the row or the byte bound ([`RULE_FULL`]).
    pub full: u32,
    /// The rows past the age bound ([`RULE_EXPIRED`]).
    pub expired: u32,
}

/// `deferred_total`: the kept count and byte sum of `deferred` (critic L9), kept by the schema's
/// triggers in the statement that changes a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeferredTotal {
    /// The rows.
    pub rows: i64,
    /// The sum of `length(envelope)`.
    pub bytes: i64,
}

/// The WARN lines of a zone whose bounds dropped rows: one per rule with its count, written after the
/// commit (none when the zone rolls back). The zone's owner calls it ONCE with the sum of its
/// [`add_deferred`] results (the batch zone, A5a), so a full store gives one line per batch, not 30.
pub(crate) fn warn_dropped(tx: &mut Tx<'_>, dropped: Dropped) {
    for (rule, n) in [(RULE_EXPIRED, dropped.expired), (RULE_FULL, dropped.full)] {
        if n > 0 {
            tx.on_commit(move || {
                tracing::warn!(rule, dropped = n, "deferred envelopes dropped by a bound");
            });
        }
    }
}

/// The kept count and sum (the one `deferred_total` row; a file without it is unreadable).
pub fn deferred_total(conn: &Connection) -> Result<DeferredTotal> {
    conn.query_row(
        "SELECT row_count, byte_sum FROM deferred_total WHERE id = 1",
        [],
        |r| {
            Ok(DeferredTotal {
                rows: r.get(0)?,
                bytes: r.get(1)?,
            })
        },
    )
    .optional()
    .map_err(sql("read the deferred total failed"))?
    .ok_or(StoreError::Unreadable("the deferred total unreadable"))
}

/// Deletes every row past the age bound (`received_at < now_ms - age_ms`: a row exactly `age_ms` old
/// stays) inside the caller's zone, but never a SYNC kind ([`EXEMPT_KINDS`], round B M-2 = S2 (a));
/// returns how many went.
fn expire_in(tx: &Tx<'_>, now_ms: i64, bounds: &DeferredBounds) -> Result<u32> {
    let cutoff = now_ms.saturating_sub(bounds.age_ms);
    let [a, b, c] = EXEMPT_KINDS;
    let n = tx
        .execute(
            "DELETE FROM deferred WHERE received_at < ?1 AND kind NOT IN (?2, ?3, ?4)",
            params![cutoff, a, b, c],
        )
        .map_err(sql("expire deferred rows failed"))?;
    u32::try_from(n).map_err(|_| StoreError::Failed("expire deferred rows failed"))
}

/// The row the bounds take next (round B, M-2 = S2 (c)): the OLDEST row of a LOW-VALUE kind
/// ([`LOW_VALUE_KINDS`]: one seek per kind on `deferred_by_kind`), else the oldest row of any kind but the
/// exempt ones ([`EXEMPT_KINDS`]: a scan by id that stops at the first such row; the exempt rows are few);
/// `None` when only exempt rows are left.
fn next_to_evict(tx: &Tx<'_>) -> Result<Option<i64>> {
    const OP: &str = "defer an envelope failed";
    let mut low: Option<i64> = None;
    let mut first = tx
        .prepare_cached("SELECT id FROM deferred WHERE kind = ?1 ORDER BY id LIMIT 1")
        .map_err(sql(OP))?;
    for kind in LOW_VALUE_KINDS {
        let id: Option<i64> = first
            .query_row(params![kind], |r| r.get(0))
            .optional()
            .map_err(sql(OP))?;
        low = match (low, id) {
            (Some(l), Some(i)) => Some(l.min(i)),
            (l, i) => l.or(i),
        };
    }
    if low.is_some() {
        return Ok(low);
    }
    let [a, b, c] = EXEMPT_KINDS;
    tx.prepare_cached("SELECT id FROM deferred WHERE kind NOT IN (?1, ?2, ?3) ORDER BY id LIMIT 1")
        .map_err(sql(OP))?
        .query_row(params![a, b, c], |r| r.get(0))
        .optional()
        .map_err(sql(OP))
}

/// Defers ONE envelope inside the caller's zone (the reflection loop's batch zone, A5a; TH6's leader
/// receive): first the rows past the age bound go (never a sync kind), then the row is written at
/// `received_at = now_ms`, then, while the kept count is over `bounds.rows` or the kept sum over
/// `bounds.bytes`, one row goes in THE EVICTION ORDER (round B, M-2 = S2): the oldest row of a LOW-VALUE
/// kind ([`LOW_VALUE_KINDS`]: receipts, read / sent updates, calls), else the oldest of the rest, never a
/// SYNC kind ([`EXEMPT_KINDS`]); the new row too, when it is the one the order names. The exempt rows count
/// toward the bounds but are never taken: when only they are left the loop ends over the bound (they are
/// few; the kept sum is checked against the table then). Returns what went; the zone's owner writes the
/// WARN lines once for the zone ([`warn_dropped`]). A row the schema refuses (a bad kind, version 0, an
/// empty or too long envelope, a bad source) fails the zone by name and names no input.
pub fn add_deferred(
    tx: &mut Tx<'_>,
    row: &NewDeferred<'_>,
    now_ms: i64,
    bounds: &DeferredBounds,
) -> Result<Dropped> {
    const OP: &str = "defer an envelope failed";
    let expired = expire_in(tx, now_ms, bounds)?;
    tx.execute(
        "INSERT INTO deferred (source, kind, map_version, server_at, flags, received_at, envelope)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            row.source.as_str(),
            row.kind,
            row.map_version,
            to_sql_u64(row.server_at),
            row.flags,
            now_ms,
            row.envelope,
        ],
    )
    .map_err(sql(OP))?;
    let mut full = 0u32;
    loop {
        let total = deferred_total(tx)?;
        if total.rows <= bounds.rows && total.bytes <= bounds.bytes {
            break;
        }
        let Some(id) = next_to_evict(tx)? else {
            // Only exempt rows are left: they stay over the bound, unless the kept sum is wrong (a table
            // with fewer rows than it says).
            let counted: i64 = tx
                .query_row("SELECT count(*) FROM deferred", [], |r| r.get(0))
                .map_err(sql(OP))?;
            if counted != total.rows {
                return Err(StoreError::Unreadable("the deferred total unreadable"));
            }
            break;
        };
        if !delete_deferred(tx, id)? {
            return Err(StoreError::Unreadable("the deferred total unreadable"));
        }
        full += 1;
    }
    Ok(Dropped { full, expired })
}

/// The start's age bound in its own zone (failpoints `INSIDE_EXPIRE` / `AFTER_EXPIRE`): every row past
/// `bounds.age_ms` goes but the sync kinds ([`EXEMPT_KINDS`]), so a row outlives its age by at most the
/// time the account neither connects nor defers ([`add_deferred`] applies the bound too). The actor runs it
/// once per start, after the first `ServerInfo` whose clock check passed (round B, S9: never before the
/// first socket, never under "Connect Anyway").
pub fn expire_deferred(db: &ZoneDb, now_ms: i64, bounds: &DeferredBounds) -> Result<u32> {
    db::zone(db, EXPIRE_POINTS, |tx| {
        let expired = expire_in(tx, now_ms, bounds)?;
        warn_dropped(tx, Dropped { full: 0, expired });
        Ok(expired)
    })
}

/// The start's cheap check (TH5b S-L1, [`has_deferred_below`]): an EXISTS over the covering index
/// `deferred_by_version` (0003's), one search that stops at the index's first entry (the plan test below).
const DEFERRED_BELOW: &str = "SELECT EXISTS(SELECT 1 FROM deferred WHERE map_version < ?1)";

/// Whether any `deferred` row is BELOW `map_version` (TH5b S-L1): the re-map asks it before its first page,
/// so a start with nothing to map reads no page and decrypts nothing, whatever the table holds (one search of
/// `deferred_by_version`; the page query goes by rowid for its `ORDER BY id`).
pub(crate) fn has_deferred_below(conn: &Connection, map_version: u32) -> Result<bool> {
    conn.prepare_cached(DEFERRED_BELOW)
        .and_then(|mut stmt| stmt.query_row([map_version], |r| r.get(0)))
        .map_err(sql("read deferred rows failed"))
}

/// The re-map's next page (decision M3): the rows with `map_version` BELOW `map_version` (a row AT
/// it is not mapped again) and an id above `after_id`, by id, at most [`REMAP_PAGE`]. The first page
/// is `after_id = 0`; the next starts after the last id of this one. Reads inside a zone too.
pub fn deferred_page(conn: &Connection, map_version: u32, after_id: i64) -> Result<Vec<Deferred>> {
    const OP: &str = "read deferred rows failed";
    let mut stmt = conn
        .prepare_cached(
            "SELECT id, source, kind, map_version, server_at, flags, received_at, envelope
             FROM deferred WHERE map_version < ?1 AND id > ?2 ORDER BY id LIMIT ?3",
        )
        .map_err(sql(OP))?;
    let limit = i64::try_from(REMAP_PAGE).map_err(|_| StoreError::Failed(OP))?;
    let mut rows = stmt
        .query(params![map_version, after_id, limit])
        .map_err(sql(OP))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(sql(OP))? {
        let source: String = r.get(1).map_err(sql(OP))?;
        let envelope: Vec<u8> = r.get(7).map_err(sql(OP))?;
        out.push(Deferred {
            id: r.get(0).map_err(sql(OP))?,
            source: DeferredSource::parse(&source)
                .ok_or(StoreError::Unreadable("a deferred row unreadable"))?,
            kind: r.get(2).map_err(sql(OP))?,
            map_version: r.get(3).map_err(sql(OP))?,
            server_at: from_sql_u64(r.get(4).map_err(sql(OP))?),
            flags: r.get(5).map_err(sql(OP))?,
            received_at: r.get(6).map_err(sql(OP))?,
            envelope: Zeroizing::new(envelope),
        });
    }
    Ok(out)
}

/// Deletes the row `id` inside the caller's zone (the re-map mapped it: the zone that writes its
/// events). Returns whether it was there.
pub fn delete_deferred(tx: &Tx<'_>, id: i64) -> Result<bool> {
    let n = tx
        .execute("DELETE FROM deferred WHERE id = ?1", params![id])
        .map_err(sql("delete a deferred row failed"))?;
    Ok(n == 1)
}

/// The row `id` was deferred again by a later build: its `kind` and `map_version` become this build's,
/// inside the caller's zone (the re-map's), so it is not mapped again at every start. Returns whether it
/// was there. The schema refuses version 0 and a bad kind.
pub fn set_deferred_version(
    tx: &Tx<'_>,
    id: i64,
    kind: &'static str,
    map_version: u32,
) -> Result<bool> {
    let n = tx
        .execute(
            "UPDATE deferred SET kind = ?2, map_version = ?3 WHERE id = ?1",
            params![id, kind, map_version],
        )
        .map_err(sql("defer a row again failed"))?;
    Ok(n == 1)
}

/// TH5b R1, THE CLEAR: the user cleared (or deleted) the conversation `conv` on this device, and the core sends
/// `Command::ClearConversation` for it. ONE zone ([`CLEAR_POINTS`]): every `deferred` row (of either source: TH6 decided
/// that a `csp` row of the chat goes too) whose envelope belongs to `conv` ([`delete_deferred_of`]) is DELETED, whatever its time (every row there
/// arrived before the command is handled: a clear means "what came before this"), and `CommandPersisted { cmd }`
/// is written to `core_outbox` in the SAME zone, so a kill leaves both or neither and the core's command stays
/// pending until both are there (the core sends it again at the next start). Threema's D2D syncs no clear, so
/// nothing goes to the other devices; the command's anchors and `remove` change nothing here. The deletes
/// go through the delete trigger (the kept sum stays exact). THE SAME ZONE takes the chat's unsent rows of the send queue at or
/// below `before_ms`, except those in `attached` (TH6a, decision R5, `queue::clear_unsent`; `before_ms` `None`: the clear removed
/// no message a person sent, so it takes none). Returns how many `deferred` rows went. The actor calls it; the kill loop of
/// `tests/kill.rs` too.
#[doc(hidden)]
pub fn clear_deferred(
    db: &ZoneDb,
    conv: &ConvKey,
    before_ms: Option<i64>,
    attached: &[i64],
    cmd: CmdId,
    now_ms: i64,
    wake: &Rc<Notify>,
) -> Result<u32> {
    // Nothing to take of the queue when the clear removed no message a person sent, or the chat has no form there.
    let unsent = before_ms.zip(crate::send::attempt::to_of(conv));
    db::zone(db, CLEAR_POINTS, |tx| {
        let deleted = delete_deferred_of(tx, conv)?;
        if let Some((before_ms, to)) = &unsent {
            let conversation = crate::send::prepare::conversation_bytes(to);
            let sends = crate::send::queue::clear_unsent(tx, &conversation, *before_ms, attached)?;
            tracing::debug!(sends, "a cleared chat's unsent rows are deleted: {sends}");
        }
        let confirmed = BackendEvent::CommandPersisted { cmd };
        outbox::append_events(tx, std::slice::from_ref(&confirmed), now_ms, wake)?;
        Ok(deleted)
    })
}

/// The deletes of [`clear_deferred`] inside the caller's zone: the rows of both sources (TH6 decided: a `csp` row holds
/// the same `d2d.Envelope` form, the leader's `IncomingMessage`) in pages of
/// [`REMAP_PAGE`] by id (a page of 64 KiB rows is 2 MiB: the 256 MiB table is never read at once), each
/// decoded and placed by `reflect::row_conversation` (the rules of the map's `place`; a list, a sync, an
/// update, a row that does not decode: no conversation, KEPT), the rows of `conv` deleted one by one.
fn delete_deferred_of(tx: &Tx<'_>, conv: &ConvKey) -> Result<u32> {
    const OP: &str = "clear deferred rows failed";
    let limit = i64::try_from(REMAP_PAGE).map_err(|_| StoreError::Failed(OP))?;
    let mut after = 0i64;
    let mut deleted = 0u32;
    loop {
        let page: Vec<(i64, Zeroizing<Vec<u8>>)> = {
            let mut stmt = tx
                .prepare_cached(
                    "SELECT id, envelope FROM deferred WHERE id > ?1 ORDER BY id LIMIT ?2",
                )
                .map_err(sql(OP))?;
            let mut rows = stmt.query(params![after, limit]).map_err(sql(OP))?;
            let mut page = Vec::new();
            while let Some(r) = rows.next().map_err(sql(OP))? {
                let id: i64 = r.get(0).map_err(sql(OP))?;
                let envelope: Vec<u8> = r.get(1).map_err(sql(OP))?;
                page.push((id, Zeroizing::new(envelope)));
            }
            page
        };
        let Some(last) = page.last().map(|(id, _)| *id) else {
            return Ok(deleted);
        };
        for (id, envelope) in &page {
            if crate::reflect::row_conversation(envelope).as_ref() == Some(conv) {
                if !delete_deferred(tx, *id)? {
                    return Err(StoreError::Failed(OP));
                }
                deleted += 1;
            }
        }
        after = last;
    }
}

/// A batch of deferred rows in ONE batch zone (failpoints `INSIDE_REFLECTED` / `AFTER_REFLECTED`), as
/// the reflection loop writes them (without its nonces and `core_outbox` rows): for the kill loop and
/// the tests of the bounds. Returns what the bounds dropped in all.
#[doc(hidden)]
pub fn add_deferred_batch(
    db: &ZoneDb,
    rows: &[NewDeferred<'_>],
    now_ms: i64,
    bounds: &DeferredBounds,
) -> Result<Dropped> {
    db::zone(db, REFLECTED_POINTS, |tx| {
        let mut all = Dropped::default();
        for row in rows {
            let d = add_deferred(tx, row, now_ms, bounds)?;
            all.full += d.full;
            all.expired += d.expired;
        }
        warn_dropped(tx, all);
        Ok(all)
    })
}

/// One step of a re-map zone: the row was mapped (deleted) or deferred again (its new kind and version).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemapStep {
    /// [`delete_deferred`].
    Mapped(i64),
    /// [`set_deferred_version`].
    DeferredAgain {
        /// The row.
        id: i64,
        /// The rule of this build.
        kind: &'static str,
        /// This build's MAP_VERSION.
        map_version: u32,
    },
}

/// The steps of ONE re-map zone (failpoints `INSIDE_REMAP` / `AFTER_REMAP`), without the events A8 writes
/// with them: for the kill loop. A row that is not there fails the zone.
#[doc(hidden)]
pub fn remap_deferred_steps(db: &ZoneDb, steps: &[RemapStep]) -> Result<()> {
    db::zone(db, REMAP_POINTS, |tx| {
        for step in steps {
            let there = match *step {
                RemapStep::Mapped(id) => delete_deferred(tx, id)?,
                RemapStep::DeferredAgain {
                    id,
                    kind,
                    map_version,
                } => set_deferred_version(tx, id, kind, map_version)?,
            };
            if !there {
                return Err(StoreError::Failed("re-map a deferred row failed"));
            }
        }
        Ok(())
    })
}

/// THE RE-MAP (A8, decision M3) of the file's deferred rows as if this build's `MAP_VERSION` were
/// `version`: the rows BELOW `version` mapped again in zones of at most [`REMAP_PAGE`] rows (the REMAP
/// points), their events in `core_outbox` (always a catch-up), each row deleted or given `version` in the
/// zone of its events (`reflect::remap`). The own ID is the file's identity's. The actor runs the same
/// re-map at its start with the real version; tests map a version-1 row with this build's table through
/// this door. Nothing in the application calls it.
#[doc(hidden)]
pub fn remap_with(db: &ZoneDb, version: u32, now_ms: i64, wake: &Rc<Notify>) -> Result<Remapped> {
    remap_with_cancel(db, version, now_ms, wake, &CancellationToken::new())
}

/// [`remap_with`] under the actor's cancel token (round A, S5): the re-map checks `cancel` before every
/// page and returns what it did so far once it is cancelled. Nothing in the application calls it.
#[doc(hidden)]
pub fn remap_with_cancel(
    db: &ZoneDb,
    version: u32,
    now_ms: i64,
    wake: &Rc<Notify>,
    cancel: &CancellationToken,
) -> Result<Remapped> {
    let identity = db
        .read(load_identity)?
        .ok_or(StoreError::Failed("re-map a deferred row failed"))?;
    crate::reflect::remap::run(db, &identity.threema_id, version, now_ms, wake, cancel)
}

#[cfg(test)]
mod tests {
    use kourier_db::DbKey;

    use super::*;

    /// TH5b S-L1: the start's cheap check is ONE search of the covering index `deferred_by_version` (0003's,
    /// the frozen schema of a migrated file), never a scan of `deferred`: the code's own statement, planned by
    /// SQLite. (A mutant that writes it without the version term scans.)
    #[test]
    fn the_starts_check_for_rows_below_a_version_is_one_search_of_the_version_index() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let db = open(
            &dir.path().join("backend.db"),
            &DbKey::from_bytes([0x5e; 32]),
        )
        .expect("a migrated file");
        let plan: Vec<String> = db
            .read(|conn: &Connection| {
                let mut stmt = conn
                    .prepare(&format!("EXPLAIN QUERY PLAN {DEFERRED_BELOW}"))
                    .map_err(sql("plan"))?;
                // As many values as the statement has parameters (a statement without the term has none).
                let values = std::iter::repeat_n(2_u32, stmt.parameter_count());
                stmt.query_map(kourier_db::rusqlite::params_from_iter(values), |r| {
                    r.get::<_, String>(3)
                })
                .map_err(sql("plan"))?
                .collect::<std::result::Result<Vec<String>, _>>()
                .map_err(sql("plan"))
            })
            .expect("the plan");
        // The outer `SELECT EXISTS(..)` has no table ("SCAN CONSTANT ROW"); `deferred` itself is never scanned.
        assert!(
            !plan.iter().any(|line| line.starts_with("SCAN deferred")),
            "the check scans deferred: {plan:?}"
        );
        assert_eq!(
            plan,
            [
                "SCAN CONSTANT ROW",
                "SCALAR SUBQUERY 1",
                "SEARCH deferred USING COVERING INDEX deferred_by_version (map_version<?)"
            ],
            "the check is not one search of the version index"
        );
        // And it answers: an empty table holds nothing below any version.
        assert_eq!(
            db.read(|conn: &Connection| has_deferred_below(conn, 2)),
            Ok(false)
        );
    }

    /// S-M2: the set the send steps look in is the creator and the members of every stored group whose user state is
    /// not `LEFT` or `KICKED` (an absent state is `MEMBER`), and nobody of a group the user left or was kicked from,
    /// unless another group the user is in names them: the semantics of the per-identity walk it replaced.
    #[test]
    fn the_identities_in_groups_are_those_of_the_groups_the_user_is_in() {
        use d2d_sync::group::UserState;
        use libthreema::protobuf::common::Identities;

        let dir = tempfile::tempdir().expect("a temp dir");
        let db = open(
            &dir.path().join("backend.db"),
            &DbKey::from_bytes([0x5e; 32]),
        )
        .expect("a migrated file");
        let group = |members: Option<&[&str]>, state: Option<UserState>| d2d_sync::Group {
            member_identities: members.map(|m| Identities {
                identities: m.iter().map(|i| (*i).to_owned()).collect(),
            }),
            user_state: state.map(|s| s as i32),
            ..Default::default()
        };
        let none = db
            .read(|conn: &Connection| identities_in_groups(conn))
            .expect("the look at no groups");
        assert!(none.is_empty(), "the identities of a file with no group");
        db.zone(|tx| {
            // The user's state absent (MEMBER): the creator and its members count.
            put_group(
                tx,
                "CREATOR1",
                1,
                &group(Some(&["MEMBER01", "MEMBER02"]), None),
                0,
            )?;
            // MEMBER written out.
            put_group(
                tx,
                "CREATOR2",
                2,
                &group(Some(&["MEMBER03"]), Some(UserState::Member)),
                0,
            )?;
            // No member list at all: the creator alone.
            put_group(tx, "CREATOR3", 3, &group(None, None), 0)?;
            // LEFT and KICKED: nobody of them counts.
            put_group(
                tx,
                "CREATOR4",
                4,
                &group(Some(&["MEMBER04"]), Some(UserState::Left)),
                0,
            )?;
            put_group(
                tx,
                "CREATOR5",
                5,
                &group(Some(&["MEMBER05"]), Some(UserState::Kicked)),
                0,
            )?;
            // A member of a group the user left who is also in one the user is in counts through the second.
            put_group(
                tx,
                "CREATOR6",
                6,
                &group(Some(&["MEMBER04", "MEMBER06"]), Some(UserState::Member)),
                0,
            )?;
            Ok::<(), StoreError>(())
        })
        .expect("the groups are stored");
        let found = db
            .read(|conn: &Connection| identities_in_groups(conn))
            .expect("the look at the groups");
        let mut found: Vec<String> = found.into_iter().collect();
        found.sort_unstable();
        assert_eq!(
            found,
            [
                "CREATOR1", "CREATOR2", "CREATOR3", "CREATOR6", "MEMBER01", "MEMBER02", "MEMBER03",
                "MEMBER04", "MEMBER06",
            ],
            "the identities of the groups the user is in"
        );
    }
}
