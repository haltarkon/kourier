//! Turning a fetch into backend state and core events (§5.6, §6.8): [`prepare`] fetches,
//! decrypts and downloads (network, outside any zone); [`apply`] is the zone body that stores
//! the new revision, snapshot, endorsements and avatar and returns the events that go into
//! `core_outbox` in the *same* zone: the `ConversationUpsert{title, avatar, members (full),
//! timer, features}` and one `System` message per event of every change since the stored
//! revision (none on the first fetch, Desktop drops the "you joined" history too). A kill after
//! this commit leaves the events in `core_outbox`, so the core gets them on the next start; a
//! kill before leaves the old revision, so the next fetch computes them again.
//!
//! Concurrent fetches of one group (a lane step and the refresh job) are applied in order of
//! revision: a stale one changes nothing, an overlapping one adds only the changes after the
//! stored revision.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_db::Tx;
use kourier_model::{
    BlobRef, Body, ConversationPatch, Member, MemberList, MemberRole, Origin, Patch, SystemEvent,
    UserId,
};
use libsignal_core::{Aci, ServiceId};

use super::decode::{self, DecodedChange, DecodedState};
use super::fetch::{self, Fetched};
use super::{GroupKeys, GroupRow, Membership, Snapshot, system_local};
use crate::actor::Shared;
use crate::avatar;
use crate::db::Account;
use crate::error::Result;
use crate::net::SignalTransport;
use crate::service_id::{aci_user_id, service_id_user_id};
use crate::{features, peers, profile};

/// Validated send endorsements (`GroupSendEndorsementsResponse`) and their expiry.
#[derive(Debug)]
pub(crate) struct Endorsements {
    /// The serialized response (S11 combines tokens from it).
    pub(crate) bytes: Vec<u8>,
    /// Expiry (ms).
    pub(crate) expire_at: i64,
}

/// What happens to the group's avatar.
#[derive(Debug)]
pub(crate) enum AvatarUpdate {
    /// The core has the current one.
    Unchanged,
    /// A new file for CDN0 `key`, written but not recorded yet.
    Set {
        /// The CDN0 key.
        key: String,
        /// The file.
        written: avatar::Written,
    },
    /// The group has no avatar any more.
    Clear,
}

/// What a fetch found.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The current state and the changes after the stored revision.
    Current {
        /// The decrypted state.
        state: Box<DecodedState>,
        /// The changes, oldest first.
        changes: Vec<DecodedChange>,
        /// New endorsements, when the server sent valid ones.
        endorsements: Option<Endorsements>,
        /// The avatar.
        avatar: AvatarUpdate,
    },
    /// We are not a member (removed, or we left on another device). `change` is the verified
    /// change of the message that told us, when it carried one.
    NotMember {
        /// The embedded change.
        change: Option<DecodedChange>,
    },
    /// The group no longer exists.
    Gone,
}

/// A fetched group, ready for [`apply`].
#[derive(Debug)]
pub(crate) struct Update {
    /// The group.
    pub(crate) keys: GroupKeys,
    /// The result.
    pub(crate) outcome: Outcome,
}

impl Update {
    /// The avatar file written for this update (below `blobs/`), if any.
    pub(crate) fn written_path(&self) -> Option<PathBuf> {
        match &self.outcome {
            Outcome::Current {
                avatar: AvatarUpdate::Set { written, .. },
                ..
            } => Some(written.blob.path.clone()),
            _ => None,
        }
    }

    /// Removes a written avatar file that no zone recorded (the zone failed).
    pub(crate) fn discard(&self, blob_dir: &Path) {
        if let Outcome::Current {
            avatar: AvatarUpdate::Set { written, .. },
            ..
        } = &self.outcome
        {
            avatar::discard(blob_dir, written);
        }
    }
}

/// The message that triggered a fetch: system messages sort right before it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Trigger {
    /// Its sent timestamp (ms).
    pub(crate) timestamp: u64,
    /// When stage 1 stored it (ms).
    pub(crate) received_at: i64,
    /// Its arrival counter.
    pub(crate) arrival: u64,
    /// It is part of the catch-up.
    pub(crate) catch_up: bool,
}

/// Fetches, decrypts and downloads what [`apply`] needs for the group of `keys` whose stored
/// row is `row`. `embedded` is the `groupV2.groupChange` of the message that asked for the
/// fetch, used to tell who removed us when the server no longer lets us read the group.
pub(crate) async fn prepare<T: SignalTransport>(
    shared: &Shared<T>,
    socket: &Arc<T::Socket>,
    account: &Account,
    keys: &GroupKeys,
    row: Option<&GroupRow>,
    embedded: Option<&[u8]>,
) -> Result<Update> {
    let known = row
        .filter(|r| r.snapshot.is_some())
        .and_then(|r| r.revision);
    let expire_at = row.and_then(|r| r.endorsements_expire_at);
    let fetched = fetch::fetch(shared, socket, account, keys, known, expire_at).await?;
    let outcome = match fetched {
        Fetched::Current {
            state,
            changes,
            endorsements,
        } => {
            let state = decode::state(keys, &state);
            let mut changes: Vec<DecodedChange> = changes
                .iter()
                .filter_map(|change| decode::logged_change(keys, change, account.aci))
                .filter(|change| known.is_none_or(|known| change.revision > known))
                .collect();
            if changes.is_empty()
                && let (Some(known), Some(change)) = (
                    known,
                    embedded.and_then(|b| embedded_change(shared, keys, account, b)),
                )
                && change.revision > known
            {
                // The logs were closed to us (we were removed): the message's own signed change
                // says what happened.
                changes.push(change);
            }
            let endorsements = endorsements.and_then(|bytes| {
                validate_endorsements(shared, keys, &state.snapshot, account.aci, bytes)
            });
            let avatar = avatar_update(shared, keys, &state.snapshot, row).await;
            Outcome::Current {
                state: Box::new(state),
                changes,
                endorsements,
                avatar,
            }
        }
        Fetched::NotMember => Outcome::NotMember {
            change: embedded.and_then(|b| embedded_change(shared, keys, account, b)),
        },
        Fetched::Gone => Outcome::Gone,
    };
    Ok(Update {
        keys: *keys,
        outcome,
    })
}

/// The verified, decoded change embedded in a group message.
fn embedded_change<T: SignalTransport>(
    shared: &Shared<T>,
    keys: &GroupKeys,
    account: &Account,
    bytes: &[u8],
) -> Option<DecodedChange> {
    let server = shared.zk_params.as_ref()?;
    let actions = decode::embedded_change(server, keys, bytes)?;
    Some(decode::actions(keys, &actions, account.aci))
}

/// Checks the endorsements against the member list (`GroupSendEndorsementsResponse::
/// receive_with_service_ids`, which also rejects an expiry outside 2 h … 7 days); invalid ones
/// are dropped (S11 then refreshes the group before a send).
fn validate_endorsements<T: SignalTransport>(
    shared: &Shared<T>,
    keys: &GroupKeys,
    snapshot: &Snapshot,
    own: Aci,
    bytes: Vec<u8>,
) -> Option<Endorsements> {
    let server = shared.zk_params.as_ref()?;
    let response: zkgroup::groups::GroupSendEndorsementsResponse =
        zkgroup::deserialize(&bytes).ok()?;
    let expiration = response.expiration();
    let members: Vec<ServiceId> = snapshot
        .members
        .iter()
        .map(|m| ServiceId::from(Aci::from(m.aci)))
        .collect();
    if !members.contains(&ServiceId::from(own)) {
        return None;
    }
    let now = zkgroup::Timestamp::from_epoch_seconds(shared.now_ms() / 1000);
    match response.receive_with_service_ids_single_threaded(members, now, &keys.secret, server) {
        Ok(_) => Some(Endorsements {
            bytes,
            expire_at: i64::try_from(expiration.epoch_seconds())
                .unwrap_or(i64::MAX / 1000)
                .saturating_mul(1000),
        }),
        Err(_) => {
            tracing::warn!(
                group = %super::log_id(&keys.id),
                "group send endorsements did not verify"
            );
            None
        }
    }
}

/// Downloads the avatar when the core does not have the current one. A failed download keeps
/// the old one (the next fetch tries again): an avatar never holds back messages.
async fn avatar_update<T: SignalTransport>(
    shared: &Shared<T>,
    keys: &GroupKeys,
    snapshot: &Snapshot,
    row: Option<&GroupRow>,
) -> AvatarUpdate {
    let stored = row.and_then(|r| r.avatar_key.as_deref());
    let in_core = row.is_some_and(|r| r.features_hash.is_some() && r.avatar_blob.is_some());
    let Some(key) = snapshot.avatar.as_deref() else {
        return if stored.is_some() {
            AvatarUpdate::Clear
        } else {
            AvatarUpdate::Unchanged
        };
    };
    if stored == Some(key) && in_core {
        return AvatarUpdate::Unchanged;
    }
    let image = match fetch::avatar(shared, keys, key).await {
        Ok(image) => image,
        Err(e) => {
            tracing::warn!(error = %e, group = %super::log_id(&keys.id), "group avatar download failed");
            return AvatarUpdate::Unchanged;
        }
    };
    match avatar::write(&shared.blob_dir, &image) {
        Ok(written) => AvatarUpdate::Set {
            key: key.to_owned(),
            written,
        },
        Err(e) => {
            tracing::warn!(error = %e, "storing a group avatar failed");
            AvatarUpdate::Unchanged
        }
    }
}

/// What [`apply`] did.
#[derive(Debug, Default)]
pub(crate) struct Applied {
    /// Core events for `core_outbox` (written by the caller in the same zone).
    pub(crate) events: Vec<BackendEvent>,
    /// Profile fetches were queued (wake the job runner after the commit).
    pub(crate) profile_fetches: bool,
}

/// The membership `snapshot` gives us.
fn membership_of(snapshot: &Snapshot, account: &Account) -> Membership {
    if snapshot.is_member(account.aci) {
        Membership::Member
    } else if snapshot.is_pending(&ServiceId::from(account.aci).service_id_string())
        || snapshot.is_pending(&ServiceId::from(account.pni).service_id_string())
    {
        Membership::Pending
    } else {
        Membership::Left
    }
}

/// The core's member list of `snapshot`: full members, invitees and requesters.
pub(crate) fn member_list(snapshot: &Snapshot) -> MemberList {
    let mut upsert = Vec::with_capacity(snapshot.members.len() + snapshot.pending.len());
    for member in &snapshot.members {
        if let Ok(user) = aci_user_id(Aci::from(member.aci)) {
            upsert.push(Member {
                user,
                role: if member.admin {
                    MemberRole::Admin
                } else {
                    MemberRole::Member
                },
            });
        }
    }
    for pending in &snapshot.pending {
        if let Some(user) = ServiceId::parse_from_service_id_string(&pending.service_id)
            .and_then(|s| service_id_user_id(s).ok())
        {
            upsert.push(Member {
                user,
                role: MemberRole::Pending,
            });
        }
    }
    for requesting in &snapshot.requesting {
        if let Ok(user) = aci_user_id(Aci::from(*requesting)) {
            upsert.push(Member {
                user,
                role: MemberRole::Requesting,
            });
        }
    }
    MemberList {
        full: true,
        upsert,
        remove: Vec::new(),
    }
}

/// The row `keys` has now (inserted when new).
fn current_row(tx: &Tx<'_>, keys: &GroupKeys, now: i64) -> Result<GroupRow> {
    super::ensure(tx, keys, now)?;
    super::load_by_master(tx, &keys.master)?
        .ok_or(crate::error::Error::Internal("group row vanished"))
}

/// Applies `update` in the caller's zone and returns the core events to write with it.
pub(crate) fn apply(
    tx: &mut Tx<'_>,
    account: &Account,
    blob_dir: &Path,
    update: Update,
    trigger: Option<&Trigger>,
    now: i64,
) -> Result<Applied> {
    let keys = update.keys;
    let row = current_row(tx, &keys, now)?;
    match update.outcome {
        Outcome::Current {
            state,
            changes,
            endorsements,
            avatar,
        } => apply_current(
            tx,
            account,
            blob_dir,
            &row,
            Current {
                state: *state,
                changes,
                endorsements,
                avatar,
            },
            trigger,
            now,
        ),
        Outcome::NotMember { change } => {
            apply_left(tx, account, &row, Membership::Left, change, trigger, now)
        }
        Outcome::Gone => apply_left(tx, account, &row, Membership::Gone, None, trigger, now),
    }
}

struct Current {
    state: DecodedState,
    changes: Vec<DecodedChange>,
    endorsements: Option<Endorsements>,
    avatar: AvatarUpdate,
}

#[allow(
    clippy::too_many_arguments,
    reason = "one zone body, all inputs are distinct"
)]
fn apply_current(
    tx: &mut Tx<'_>,
    account: &Account,
    blob_dir: &Path,
    row: &GroupRow,
    current: Current,
    trigger: Option<&Trigger>,
    now: i64,
) -> Result<Applied> {
    let Current {
        state,
        changes,
        endorsements,
        avatar,
    } = current;
    let keys = &row.keys;
    let snapshot = state.snapshot;
    let stored = row.snapshot.as_ref().and(row.revision);
    if stored.is_some_and(|stored| stored > snapshot.revision) {
        // A concurrent fetch already stored a newer revision.
        discard_avatar(blob_dir, &avatar);
        return Ok(Applied::default());
    }
    let first = row.snapshot.is_none();
    let membership = membership_of(&snapshot, account);
    tx.prepare_cached(
        "UPDATE groups SET revision = ?2, state = ?3, membership = ?4, fetched_at = ?5,
           dirty = 0, updated_at = ?5 WHERE master_key = ?1",
    )?
    .execute((
        &keys.master[..],
        snapshot.revision,
        snapshot.to_json()?,
        membership.as_str(),
        now,
    ))?;
    if let Some(endorsements) = &endorsements {
        tx.prepare_cached(
            "UPDATE groups SET endorsements = ?2, endorsements_expire_at = ?3
             WHERE master_key = ?1",
        )?
        .execute((
            &keys.master[..],
            &endorsements.bytes,
            endorsements.expire_at,
        ))?;
    }

    // Member profile keys: a major source of contacts' keys (Desktop's `applyNewMembers`).
    let mut profile_fetches = false;
    for (aci, key) in &state.profile_keys {
        if *aci == account.aci {
            continue;
        }
        let sid = ServiceId::from(*aci).service_id_string();
        if peers::store_profile_key(tx, *aci, key, now)? {
            profile::request(tx, &sid, now)?;
            profile_fetches = true;
        } else if !profile::fetched(tx, &sid)? {
            profile_fetches |= profile::queue(tx, &sid, now)?;
        }
    }

    let in_core = row.features_hash.is_some();
    let avatar_patch = record_avatar(tx, blob_dir, row, &avatar, now)?;
    let features = features::group_features(
        Some(&snapshot),
        membership,
        account,
        &features::Limits::load(tx)?,
    );
    let hash = features::hash_value(&features);
    let mut patch = ConversationPatch::new(keys.conv_key());
    patch.title = if snapshot.title.is_empty() {
        Patch::Clear
    } else {
        Patch::Set(snapshot.title.clone())
    };
    patch.avatar = avatar_patch;
    patch.members = Some(member_list(&snapshot));
    patch.timer = if snapshot.timer_secs > 0 {
        Patch::Set(std::time::Duration::from_secs(u64::from(
            snapshot.timer_secs,
        )))
    } else {
        Patch::Clear
    };
    if row.features_hash != Some(hash) {
        patch.features = Some(features);
    }
    if !in_core {
        roaming(&mut patch, row, membership);
    }
    tx.prepare_cached("UPDATE groups SET features_hash = ?2 WHERE master_key = ?1")?
        .execute((&keys.master[..], hash))?;
    let mut events = vec![BackendEvent::ConversationUpsert(patch)];
    if !first {
        let after = stored.unwrap_or(0);
        let mut changes = changes;
        changes.sort_by_key(|c| c.revision);
        for change in changes.iter().filter(|c| c.revision > after) {
            events.extend(system_messages(account, keys, change, trigger, now));
        }
    }
    tracing::info!(
        group = %super::log_id(&keys.id),
        revision = snapshot.revision,
        membership = membership.as_str(),
        first,
        events = events.len(),
        "group state applied"
    );
    Ok(Applied {
        events,
        profile_fetches,
    })
}

/// The roaming flags of the Storage Service record, sent with the group's first upsert.
fn roaming(patch: &mut ConversationPatch, row: &GroupRow, membership: Membership) {
    patch.request_pending = Some(membership == Membership::Pending);
    patch.blocked = Some(row.blocked);
    patch.archived = Some(row.archived);
    patch.marked_unread = Some(row.marked_unread);
    patch.muted_until = row.muted_until.map_or(Patch::Clear, Patch::Set);
}

fn discard_avatar(blob_dir: &Path, avatar: &AvatarUpdate) {
    if let AvatarUpdate::Set { written, .. } = avatar {
        avatar::discard(blob_dir, written);
    }
}

/// Records the avatar change and returns the patch; the replaced file goes after the commit.
fn record_avatar(
    tx: &mut Tx<'_>,
    blob_dir: &Path,
    row: &GroupRow,
    avatar: &AvatarUpdate,
    now: i64,
) -> Result<Patch<BlobRef>> {
    let (key, blob, patch) = match avatar {
        AvatarUpdate::Unchanged => return Ok(Patch::Unchanged),
        AvatarUpdate::Set { key, written } => (
            Some(key.as_str()),
            Some(written.blob.path.to_string_lossy().into_owned()),
            Patch::Set(written.blob.clone()),
        ),
        AvatarUpdate::Clear => (None, None, Patch::Clear),
    };
    tx.prepare_cached(
        "UPDATE groups SET avatar_key = ?2, avatar_blob = ?3, updated_at = ?4
         WHERE master_key = ?1",
    )?
    .execute((&row.keys.master[..], key, &blob, now))?;
    if let Some(old) = row
        .avatar_blob
        .as_ref()
        .filter(|old| Some(*old) != blob.as_ref())
    {
        let file: PathBuf = blob_dir.join(old);
        tx.on_commit(move || {
            let _ = std::fs::remove_file(&file);
        });
    }
    Ok(patch)
}

/// We are not a member (or the group is gone): the stored state loses us, the core learns that
/// we cannot send, and, when the message that told us carried a verified change, its events.
fn apply_left(
    tx: &mut Tx<'_>,
    account: &Account,
    row: &GroupRow,
    membership: Membership,
    change: Option<DecodedChange>,
    trigger: Option<&Trigger>,
    now: i64,
) -> Result<Applied> {
    let keys = &row.keys;
    if row.membership == membership {
        return Ok(Applied::default());
    }
    let snapshot = row.snapshot.clone().map(|mut snapshot| {
        let own = uuid::Uuid::from(account.aci);
        snapshot.members.retain(|m| m.aci != own);
        if let Some(change) = &change {
            snapshot.revision = snapshot.revision.max(change.revision);
        }
        snapshot
    });
    let state = snapshot.as_ref().map(Snapshot::to_json).transpose()?;
    tx.prepare_cached(
        "UPDATE groups SET membership = ?2, state = coalesce(?3, state),
           revision = coalesce(?4, revision), dirty = 0, fetched_at = ?5, updated_at = ?5
         WHERE master_key = ?1",
    )?
    .execute((
        &keys.master[..],
        membership.as_str(),
        state,
        snapshot.as_ref().map(|s| s.revision),
        now,
    ))?;
    tracing::info!(
        group = %super::log_id(&keys.id),
        membership = membership.as_str(),
        "no longer in the group"
    );
    if row.features_hash.is_none() && row.snapshot.is_none() {
        // The core never had this group: nothing to tell it.
        return Ok(Applied::default());
    }
    let features = features::group_features(
        snapshot.as_ref(),
        membership,
        account,
        &features::Limits::load(tx)?,
    );
    let hash = features::hash_value(&features);
    let mut patch = ConversationPatch::new(keys.conv_key());
    if let Some(snapshot) = &snapshot {
        patch.members = Some(member_list(snapshot));
    }
    patch.features = Some(features);
    if row.features_hash.is_none() {
        roaming(&mut patch, row, membership);
    }
    tx.prepare_cached("UPDATE groups SET features_hash = ?2 WHERE master_key = ?1")?
        .execute((&keys.master[..], hash))?;
    let mut events = vec![BackendEvent::ConversationUpsert(patch)];
    if let Some(change) = &change
        && row.snapshot.is_some()
    {
        events.extend(system_messages(account, keys, change, trigger, now));
    }
    Ok(Applied {
        events,
        profile_fetches: false,
    })
}

/// The `System` messages of one change, authored by its source.
fn system_messages(
    account: &Account,
    keys: &GroupKeys,
    change: &DecodedChange,
    trigger: Option<&Trigger>,
    now: i64,
) -> Vec<BackendEvent> {
    let Some(source) = change.source else {
        return Vec::new();
    };
    let author = if source == ServiceId::from(account.aci) {
        aci_user_id(account.aci).ok()
    } else {
        service_id_user_id(source).ok()
    };
    let Some(author) = author else {
        return Vec::new();
    };
    let sent_at = trigger
        .map(|t| i64::try_from(t.timestamp).unwrap_or(i64::MAX))
        .unwrap_or(now);
    change
        .events
        .iter()
        .enumerate()
        .map(|(index, event)| {
            system_message(
                keys,
                change.revision,
                index,
                &author,
                event.clone(),
                sent_at,
                trigger,
                now,
            )
        })
        .collect()
}

#[allow(
    clippy::too_many_arguments,
    reason = "plain message fields, all distinct"
)]
fn system_message(
    keys: &GroupKeys,
    revision: u32,
    index: usize,
    author: &UserId,
    event: SystemEvent,
    sent_at: i64,
    trigger: Option<&Trigger>,
    now: i64,
) -> BackendEvent {
    let mut message = IncomingMessage::new(
        keys.conv_key(),
        system_local(&keys.id, revision, index),
        Origin::Remote {
            author: author.clone(),
            device: None,
        },
        Body::System(event),
        sent_at,
        trigger.map_or(now, |t| t.received_at),
    );
    message.arrival = trigger.map(|t| t.arrival);
    message.catch_up = trigger.is_some_and(|t| t.catch_up);
    message.silent = true;
    BackendEvent::Message(message)
}
