//! The network step of a group lane (§6.6, §6.8; Desktop's `maybeUpdateGroup` before
//! `handleDataMessage`): when a row's `groupV2` names a revision newer than the stored one, or the
//! group was never fetched, or it is dirty, the group is fetched (time-boxed) and the update
//! becomes an input of the lane step's zone. When the fetch fails, the rows are still applied
//! with the last known state if every sender is a full member under it (the group turns dirty and
//! its refresh job retries); otherwise the lane parks with backoff, holding back only this group.

use libsignal_core::{Aci, ServiceId};
use prost::Message as _;

use super::apply;
use super::{GroupKeys, GroupRow, Membership, Snapshot};
use crate::actor::Shared;
use crate::error::Error;
use crate::net::{SignalTransport, TransportError};
use crate::proto::{self, content::Content as ContentKind};
use crate::receive::KIND_CONTENT;
use crate::receive::stage2::{LaneInputs, NETWORK_STEP_TIMEOUT, NetworkFailure, Row};

/// The group context of a message: master key, revision and embedded signed change.
pub(crate) struct Context<'a> {
    /// The master key (unchecked length).
    pub(crate) master_key: &'a [u8],
    /// The revision the sender had.
    pub(crate) revision: u32,
    /// `groupV2.groupChange`.
    pub(crate) change: Option<&'a [u8]>,
}

/// The `GroupContextV2` of a `Content`: a data message, an edit, or our phone's transcript of
/// either.
pub(crate) fn context(content: &proto::Content) -> Option<Context<'_>> {
    let data = match content.content.as_ref()? {
        ContentKind::DataMessage(message) => message,
        ContentKind::EditMessage(edit) => edit.data_message.as_ref()?,
        ContentKind::SyncMessage(sync) => match sync.content.as_ref()? {
            proto::sync_message::Content::Sent(sent) => sent.message.as_ref().or(sent
                .edit_message
                .as_ref()
                .and_then(|e| e.data_message.as_ref()))?,
            _ => return None,
        },
        _ => return None,
    };
    let group = data.group_v2.as_ref()?;
    Some(Context {
        master_key: group.master_key.as_deref()?,
        revision: group.revision.unwrap_or(0),
        change: group.group_change.as_deref().filter(|c| !c.is_empty()),
    })
}

/// One row of a group lane step that carries group content.
struct GroupContent {
    /// Its index in the step's rows.
    index: usize,
    /// The revision its sender had.
    revision: u32,
    /// Its embedded signed change.
    change: Option<Vec<u8>>,
    /// Who sent it (`None` for a non-ACI sender).
    sender: Option<Aci>,
}

/// The group of a lane step and its rows that carry group content, in arrival order.
struct Need {
    keys: GroupKeys,
    rows: Vec<GroupContent>,
}

impl Need {
    /// The newest revision named and the newest embedded change, from `rows[from..]`.
    fn newest(&self, from: usize) -> (u32, Option<&[u8]>) {
        let mut revision = 0;
        let mut change = None;
        for row in self.rows.iter().skip(from) {
            if row.revision >= revision {
                revision = row.revision;
                if let Some(c) = row.change.as_deref() {
                    change = Some(c);
                }
            }
        }
        (revision, change)
    }
}

fn need(rows: &[Row]) -> Option<Need> {
    let mut found: Option<Need> = None;
    for (index, row) in rows.iter().enumerate() {
        if row.kind != KIND_CONTENT {
            continue;
        }
        let Some(content) = row
            .content
            .as_deref()
            .and_then(|bytes| proto::Content::decode(bytes).ok())
        else {
            continue;
        };
        let Some(context) = context(&content) else {
            continue;
        };
        let sender = match ServiceId::parse_from_service_id_string(&row.sender) {
            Some(ServiceId::Aci(aci)) => Some(aci),
            _ => None,
        };
        let entry = GroupContent {
            index,
            revision: context.revision,
            change: context.change.map(<[u8]>::to_vec),
            sender,
        };
        match &mut found {
            Some(need) if need.keys.master[..] == *context.master_key => need.rows.push(entry),
            // Another master key in the same lane (a hash collision of the lane key): the
            // first group decides; dispatch checks every row against its own group anyway.
            Some(_) => {}
            None => {
                let Some(keys) = GroupKeys::from_slice(context.master_key) else {
                    continue;
                };
                found = Some(Need {
                    keys,
                    rows: vec![entry],
                });
            }
        }
    }
    found
}

/// Whether the stored row is current enough for `need`.
fn is_current(row: &GroupRow, revision: u32) -> bool {
    row.snapshot.is_some()
        && !row.dirty
        && row.membership != Membership::Unknown
        && row.revision.is_some_and(|stored| stored >= revision)
}

/// Whether every sender is a full member under `snapshot` (our own transcripts count when we are
/// one).
fn all_members(snapshot: &Snapshot, senders: &[Option<Aci>]) -> bool {
    senders
        .iter()
        .all(|sender| sender.is_some_and(|aci| snapshot.is_member(aci)))
}

/// The network step of `lane`: nothing for lanes other than groups'.
pub(crate) async fn inputs<T: SignalTransport>(
    shared: &Shared<T>,
    lane: &str,
    rows: &[Row],
) -> Result<LaneInputs, NetworkFailure> {
    if !lane.starts_with("group:") {
        return Ok(LaneInputs::default());
    }
    let Some(need) = need(rows) else {
        return Ok(LaneInputs::default());
    };
    let row = shared
        .db
        .read(|c| super::load_by_master(c, &need.keys.master))
        .map_err(|e| NetworkFailure(format!("reading the group: {e}")))?;
    // Desktop handles a conversation's messages one at a time and updates the group before the
    // first one that names a newer revision: the rows before it are applied with the state they
    // were sent under (a member removed later still has their earlier messages shown), and the
    // fetch waits for the next step, which starts at that row.
    let Some(first) = need
        .rows
        .iter()
        .position(|r| !row.as_ref().is_some_and(|g| is_current(g, r.revision)))
    else {
        return Ok(LaneInputs::default());
    };
    let at = need.rows.get(first).map_or(0, |r| r.index);
    if at > 0 {
        return Ok(LaneInputs {
            limit: Some(at),
            ..LaneInputs::default()
        });
    }
    let (_revision, change) = need.newest(first);
    let Some(account) = shared.account() else {
        return Ok(LaneInputs::default());
    };
    let socket = shared.socket.borrow().clone();
    let fetched = match socket {
        Some(socket) => tokio::time::timeout(
            NETWORK_STEP_TIMEOUT,
            apply::prepare(shared, &socket, &account, &need.keys, row.as_ref(), change),
        )
        .await
        .unwrap_or_else(|_| Err(Error::Transport(TransportError::Timeout))),
        None => Err(Error::Transport(TransportError::Disconnected)),
    };
    match fetched {
        Ok(update) => Ok(LaneInputs {
            group: Some(update),
            ..LaneInputs::default()
        }),
        Err(e) => {
            let senders: Vec<Option<Aci>> = need.rows.iter().map(|r| r.sender).collect();
            let fallback = row
                .as_ref()
                .and_then(|r| r.snapshot.as_ref())
                .is_some_and(|snapshot| all_members(snapshot, &senders));
            if fallback {
                tracing::warn!(
                    error = %e,
                    group = %super::log_id(&need.keys.id),
                    "group fetch failed; applying with the last known state"
                );
                Ok(LaneInputs {
                    degraded: Some(need.keys),
                    ..LaneInputs::default()
                })
            } else {
                Err(NetworkFailure(format!(
                    "group {} fetch failed: {e}",
                    super::log_id(&need.keys.id)
                )))
            }
        }
    }
}
