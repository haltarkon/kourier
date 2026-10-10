//! Groups v2, read only in M1 (§6.8; ports `SD/ts/groups.preload.ts` `maybeUpdateGroup`,
//! `getGroupUpdates`, `decryptGroupState`, `decryptMember`, `decryptGroupChange`,
//! `SD/ts/services/groupCredentialFetcher.preload.ts` and the membership checks of
//! `SD/ts/messages/handleDataMessage.preload.ts`).
//!
//! - [`credentials`]: the 7-day window of group auth credentials and the per-request
//!   presentation (HTTP Basic auth to the Storage Service).
//! - [`fetch`]: the state or the change logs since the known revision (pages of 206), the send
//!   endorsements and the avatar from CDN0; network only, outside any zone.
//! - [`decode`]: decryption of a group state (title, description, timer, avatar key, members
//!   and their profile keys, pending and requesting members) into a [`Snapshot`], and of change
//!   actions into the system events they imply; notary-signature checks of embedded changes.
//! - [`apply`]: one zone that stores the new revision, snapshot, endorsements and avatar *and*
//!   writes the `ConversationUpsert` and `System` messages into `core_outbox` (§5.6 invariant 1),
//!   plus the group conversation features.
//! - [`lane`]: the stage-2 network step of a group lane (fetch when a message names a newer
//!   revision, or the group is unknown or dirty) and the failure policy of §6.6: apply with the
//!   last known state when every sender is a member under it (the group turns dirty and a
//!   `group_refresh` job retries), park the lane otherwise.
//!
//! Master keys come from Storage Service `GroupV2Record`s (`crate::storage`) and from incoming
//! `DataMessage.groupV2`; the group id is `GroupSecretParams::get_group_identifier()`. The core
//! knows a group as `ConvKey::Group("gv2:<base64url group id>")`, never by its master key.

pub(crate) mod apply;
pub(crate) mod credentials;
pub(crate) mod decode;
pub(crate) mod endorsements;
pub(crate) mod fetch;
pub(crate) mod lane;

#[cfg(test)]
mod tests;

use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use kourier_db::rusqlite::{Connection, OptionalExtension, Row as SqlRow};
use kourier_model::ConvKey;
use libsignal_core::Aci;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zkgroup::groups::{GroupMasterKey, GroupSecretParams};

use crate::error::{Error, Result};

/// The newest change epoch Kourier understands (Desktop's `SUPPORTED_CHANGE_EPOCH`).
pub(crate) const SUPPORTED_CHANGE_EPOCH: u32 = 7;

/// The keys of one group, derived from its master key.
#[derive(Clone, Copy)]
pub(crate) struct GroupKeys {
    /// The 32-byte master key (secret).
    pub(crate) master: [u8; 32],
    /// `GroupSecretParams::derive_from_master_key`.
    pub(crate) secret: GroupSecretParams,
    /// The group identifier (public; the core's key is built from it).
    pub(crate) id: [u8; 32],
}

impl std::fmt::Debug for GroupKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GroupKeys({})", log_id(&self.id))
    }
}

impl GroupKeys {
    /// The keys of the group with `master`.
    pub(crate) fn from_master(master: [u8; 32]) -> Self {
        let secret = GroupSecretParams::derive_from_master_key(GroupMasterKey::new(master));
        let id = secret.get_group_identifier();
        Self { master, secret, id }
    }

    /// The keys of a 32-byte master key; `None` for any other length.
    pub(crate) fn from_slice(master: &[u8]) -> Option<Self> {
        <[u8; 32]>::try_from(master).ok().map(Self::from_master)
    }

    /// The core's conversation key of this group.
    pub(crate) fn conv_key(&self) -> ConvKey {
        conv_key_of_id(&self.id)
    }

    /// The stage-2 lane of this group's rows.
    #[cfg(test)]
    pub(crate) fn lane(&self) -> String {
        crate::receive::group_lane(&self.master)
    }

    /// The serialized `GroupPublicParams` (the user name of the group auth header).
    pub(crate) fn public_params(&self) -> Vec<u8> {
        zkgroup::serialize(&self.secret.get_public_params())
    }
}

/// Every group the core has (its features were sent).
pub(crate) fn in_core(conn: &Connection) -> Result<Vec<GroupRow>> {
    let masters: Vec<Vec<u8>> = conn
        .prepare_cached("SELECT master_key FROM groups WHERE features_hash IS NOT NULL")?
        .query_map([], |r| r.get(0))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let mut rows = Vec::with_capacity(masters.len());
    for master in masters {
        if let Some(row) = load_by_master(conn, &master)? {
            rows.push(row);
        }
    }
    Ok(rows)
}

/// `ConvKey::Group("gv2:<base64url group id>")` (§3.1).
pub(crate) fn conv_key_of_id(id: &[u8]) -> ConvKey {
    ConvKey::Group(format!("gv2:{}", URL_SAFE_NO_PAD.encode(id)).into())
}

/// A group id as logs may show it: its first 4 bytes in hex (§9.1; the id is not a secret, but
/// there is no reason to print it whole).
pub(crate) fn log_id(id: &[u8]) -> String {
    let mut out = String::with_capacity(8);
    for byte in id.iter().take(4) {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The `jobs.key` of a group's refresh job: the group id in hex.
pub(crate) fn job_key(id: &[u8]) -> String {
    let mut out = String::with_capacity(id.len() * 2);
    for byte in id {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The group id of a refresh job's key.
pub(crate) fn parse_job_key(key: &str) -> Option<Vec<u8>> {
    if !key.len().is_multiple_of(2) {
        return None;
    }
    (0..key.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(key.get(i..i + 2)?, 16).ok())
        .collect()
}

/// The core-side id (`local`) of the `index`-th system message of the change to `revision` in
/// group `id` (§13 phase B open item: an id scheme for backend-generated system messages).
/// Unique per author because the group id and revision are in it.
pub(crate) fn system_local(id: &[u8], revision: u32, index: usize) -> String {
    let mut short = String::with_capacity(16);
    for byte in id.iter().take(8) {
        let _ = write!(short, "{byte:02x}");
    }
    format!("gv2:{short}:{revision}:{index}")
}

/// A group member as the snapshot keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SnapMember {
    /// The member's ACI.
    pub(crate) aci: Uuid,
    /// Administrator.
    pub(crate) admin: bool,
    /// The revision they joined at.
    #[serde(default)]
    pub(crate) joined_at: u32,
}

/// An invited member (by ACI or PNI service id string).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SnapPending {
    /// Service id string of the invitee.
    pub(crate) service_id: String,
    /// Invited as administrator.
    pub(crate) admin: bool,
}

/// Who may change what (`AccessControl.AccessRequired` values).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Access {
    /// Title, avatar, timer, description.
    pub(crate) attributes: i32,
    /// Adding members.
    pub(crate) members: i32,
    /// Joining by invite link.
    pub(crate) add_from_invite_link: i32,
}

/// The decrypted state of a group at one revision, as `groups.state` stores it (JSON). Personal
/// data: never logged.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Snapshot {
    /// The revision (`Group.version`).
    pub(crate) revision: u32,
    /// The title (may be empty).
    pub(crate) title: String,
    /// The description.
    pub(crate) description: Option<String>,
    /// The CDN0 key of the encrypted avatar.
    pub(crate) avatar: Option<String>,
    /// Disappearing-message timer in seconds (0 = off).
    pub(crate) timer_secs: u32,
    /// Full members.
    pub(crate) members: Vec<SnapMember>,
    /// Invited members.
    pub(crate) pending: Vec<SnapPending>,
    /// ACIs asking to join.
    pub(crate) requesting: Vec<Uuid>,
    /// Only administrators may send.
    pub(crate) announcements_only: bool,
    /// The group was ended for everyone.
    pub(crate) terminated: bool,
    /// Access control.
    pub(crate) access: Access,
}

impl Snapshot {
    /// The member with `aci`, if a full member.
    pub(crate) fn member(&self, aci: Aci) -> Option<&SnapMember> {
        let uuid = Uuid::from(aci);
        self.members.iter().find(|m| m.aci == uuid)
    }

    /// Whether `aci` is a full member.
    pub(crate) fn is_member(&self, aci: Aci) -> bool {
        self.member(aci).is_some()
    }

    /// Whether `service_id` (string form) is invited.
    pub(crate) fn is_pending(&self, service_id: &str) -> bool {
        self.pending.iter().any(|p| p.service_id == service_id)
    }

    /// The JSON form for `groups.state`.
    pub(crate) fn to_json(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|_| Error::Internal("group snapshot does not serialize"))
    }

    /// Parses `groups.state`.
    pub(crate) fn from_json(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(|_| Error::Decode("group snapshot"))
    }
}

/// Our place in a group (`groups.membership`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Membership {
    /// Never fetched.
    Unknown,
    /// A full member.
    Member,
    /// Invited, not joined.
    Pending,
    /// Removed or left.
    Left,
    /// The group no longer exists (404).
    Gone,
}

impl Membership {
    /// The SQL value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Member => "member",
            Self::Pending => "pending",
            Self::Left => "left",
            Self::Gone => "gone",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "member" => Self::Member,
            "pending" => Self::Pending,
            "left" => Self::Left,
            "gone" => Self::Gone,
            _ => Self::Unknown,
        }
    }
}

/// One `groups` row.
#[derive(Clone, Debug)]
pub(crate) struct GroupRow {
    /// The keys.
    pub(crate) keys: GroupKeys,
    /// The stored revision (`None` before the first fetch).
    pub(crate) revision: Option<u32>,
    /// The snapshot at that revision.
    pub(crate) snapshot: Option<Snapshot>,
    /// Our membership.
    pub(crate) membership: Membership,
    /// Blocked on our phone (Storage Service, blocked sync, request response).
    pub(crate) blocked: bool,
    /// Roaming chat state from the Storage Service.
    pub(crate) archived: bool,
    /// Roaming chat state from the Storage Service.
    pub(crate) marked_unread: bool,
    /// Roaming chat state from the Storage Service (ms).
    pub(crate) muted_until: Option<i64>,
    /// Expiry of the stored send endorsements (ms).
    pub(crate) endorsements_expire_at: Option<i64>,
    /// A fetch failed while messages were applied with the last known state.
    pub(crate) dirty: bool,
    /// CDN0 key of the avatar the core has.
    pub(crate) avatar_key: Option<String>,
    /// Its file below `blobs/`.
    pub(crate) avatar_blob: Option<String>,
    /// Hash of the features the core has; `None` while the core lacks the group.
    pub(crate) features_hash: Option<i64>,
}

const GROUP_COLUMNS: &str = "master_key, revision, state, membership, blocked, archived,
    marked_unread, muted_until, endorsements_expire_at, dirty, avatar_key, avatar_blob,
    features_hash";

fn group_row(r: &SqlRow<'_>) -> kourier_db::rusqlite::Result<(Vec<u8>, GroupRowRaw)> {
    Ok((
        r.get(0)?,
        GroupRowRaw {
            revision: r.get(1)?,
            state: r.get(2)?,
            membership: r.get(3)?,
            blocked: r.get(4)?,
            archived: r.get(5)?,
            marked_unread: r.get(6)?,
            muted_until: r.get(7)?,
            endorsements_expire_at: r.get(8)?,
            dirty: r.get(9)?,
            avatar_key: r.get(10)?,
            avatar_blob: r.get(11)?,
            features_hash: r.get(12)?,
        },
    ))
}

struct GroupRowRaw {
    revision: Option<i64>,
    state: Option<Vec<u8>>,
    membership: String,
    blocked: bool,
    archived: bool,
    marked_unread: bool,
    muted_until: Option<i64>,
    endorsements_expire_at: Option<i64>,
    dirty: bool,
    avatar_key: Option<String>,
    avatar_blob: Option<String>,
    features_hash: Option<i64>,
}

fn finish_row(master: &[u8], raw: GroupRowRaw) -> Result<Option<GroupRow>> {
    let Some(keys) = GroupKeys::from_slice(master) else {
        return Ok(None);
    };
    let snapshot = match raw.state.as_deref() {
        // A snapshot this build cannot read counts as missing: the next fetch replaces it.
        Some(bytes) => Snapshot::from_json(bytes).ok(),
        None => None,
    };
    Ok(Some(GroupRow {
        keys,
        revision: raw.revision.and_then(|r| u32::try_from(r).ok()),
        snapshot,
        membership: Membership::parse(&raw.membership),
        blocked: raw.blocked,
        archived: raw.archived,
        marked_unread: raw.marked_unread,
        muted_until: raw.muted_until,
        endorsements_expire_at: raw.endorsements_expire_at,
        dirty: raw.dirty,
        avatar_key: raw.avatar_key,
        avatar_blob: raw.avatar_blob,
        features_hash: raw.features_hash,
    }))
}

/// The group with master key `master`, if stored.
pub(crate) fn load_by_master(conn: &Connection, master: &[u8]) -> Result<Option<GroupRow>> {
    let row = conn
        .prepare_cached(&format!(
            "SELECT {GROUP_COLUMNS} FROM groups WHERE master_key = ?1"
        ))?
        .query_row([master], group_row)
        .optional()?;
    match row {
        Some((master, raw)) => finish_row(&master, raw),
        None => Ok(None),
    }
}

/// The group with id `id`, if stored.
pub(crate) fn load_by_id(conn: &Connection, id: &[u8]) -> Result<Option<GroupRow>> {
    let row = conn
        .prepare_cached(&format!(
            "SELECT {GROUP_COLUMNS} FROM groups WHERE group_id = ?1"
        ))?
        .query_row([id], group_row)
        .optional()?;
    match row {
        Some((master, raw)) => finish_row(&master, raw),
        None => Ok(None),
    }
}

/// Whether our phone accepted the group with id `id` (the Storage Service record's
/// `whitelisted`, or its message-request response): Desktop's `profileSharing` of a group.
pub(crate) fn whitelisted(conn: &Connection, id: &[u8]) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT whitelisted FROM groups WHERE group_id = ?1")?
        .query_row([id], |r| r.get(0))
        .optional()?
        .unwrap_or(false))
}

/// The master key of the group with id `id`, if stored (stage 1 routes sender-key failures and
/// group typing into the group's lane with it).
pub(crate) fn master_of(conn: &Connection, id: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(conn
        .prepare_cached("SELECT master_key FROM groups WHERE group_id = ?1")?
        .query_row([id], |r| r.get(0))
        .optional()?)
}

/// Stores the group of `keys` (from a message or a record) if it is new.
pub(crate) fn ensure(conn: &Connection, keys: &GroupKeys, now: i64) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO groups(master_key, group_id, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(master_key) DO NOTHING",
    )?
    .execute((&keys.master[..], &keys.id[..], now))?;
    Ok(())
}

/// Marks a group dirty (its fetch failed while messages were applied with the last known
/// state) and queues its refresh job.
pub(crate) fn mark_dirty(conn: &Connection, keys: &GroupKeys, now: i64) -> Result<()> {
    conn.prepare_cached("UPDATE groups SET dirty = 1, updated_at = ?2 WHERE master_key = ?1")?
        .execute((&keys.master[..], now))?;
    crate::jobs::enqueue(
        conn,
        crate::jobs::JobKind::GroupRefresh,
        &job_key(&keys.id),
        now,
        None,
    )?;
    Ok(())
}

/// The `group_refresh` job (§6.8): fetch the group and apply it in one zone with its events
/// (no triggering message: system messages are dated now).
pub(crate) async fn run_refresh<T: crate::net::SignalTransport>(
    shared: &std::rc::Rc<crate::actor::Shared<T>>,
    socket: &std::sync::Arc<T::Socket>,
    job: &crate::jobs::Job,
) -> Result<crate::jobs::Done> {
    use crate::jobs::Done;
    let account = shared.account().ok_or(Error::NotLinked)?;
    let Some(id) = parse_job_key(&job.key) else {
        return Ok(Done::Finished);
    };
    let Some(row) = shared.db.read(|c| load_by_id(c, &id))? else {
        return Ok(Done::Finished);
    };
    refresh(shared, socket, &account, &row).await?;
    Ok(Done::Finished)
}

/// Fetches the group of `row` and applies it in one zone with its events (§6.8): the refresh
/// job, and a send whose endorsements are outside their usable window (§6.7 step 5).
pub(crate) async fn refresh<T: crate::net::SignalTransport>(
    shared: &crate::actor::Shared<T>,
    socket: &std::sync::Arc<T::Socket>,
    account: &crate::db::Account,
    row: &GroupRow,
) -> Result<()> {
    let update = apply::prepare(shared, socket, account, &row.keys, Some(row), None).await?;
    let orphan = update.written_path();
    let now = shared.now();
    let applied = shared.db.zone(|tx| -> Result<bool> {
        let applied = apply::apply(tx, account, &shared.blob_dir, update, None, now)?;
        let events = crate::features::with_features(tx, account, applied.events, now)?;
        if !events.is_empty() {
            crate::outbox::append_events(tx, &events, now, &shared.wake)?;
        }
        Ok(applied.profile_fetches)
    });
    match applied {
        Ok(profile_fetches) => {
            kourier_db::failpoint::hit(crate::receive::FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT);
            if profile_fetches {
                shared.jobs_wake.notify_one();
            }
            Ok(())
        }
        Err(e) => {
            if let Some(path) = orphan {
                let _ = std::fs::remove_file(shared.blob_dir.join(path));
            }
            Err(e)
        }
    }
}

/// The group id of a core conversation key (`gv2:<base64url group id>`).
pub(crate) fn id_of_conv(conv: &ConvKey) -> Option<Vec<u8>> {
    let ConvKey::Group(key) = conv else {
        return None;
    };
    let id = URL_SAFE_NO_PAD.decode(key.strip_prefix("gv2:")?).ok()?;
    (id.len() == 32).then_some(id)
}
