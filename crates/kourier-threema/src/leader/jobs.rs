//! THE LEADER'S JOBS (TH6 L6; brief §2.11 with the critic's T-M3, T-L6 and T-L8): what the receive steps send besides
//! the message, as the send core's [`NewJob`]s. A job is written by `send::enqueue_job` INSIDE the zone that makes it
//! true, so a kill leaves the message's rows and its jobs both or neither (TH6a's door): the receipts in ZONE REFLECTED,
//! every other job in ZONE RECEIVE. The persistent queue sends them later (a job row runs at `Online`, TH6a C5).
//!
//! | Job ([`NewJob::kind`]) | When | Body (`NewBody::Raw`) | To, `only` |
//! |---|---|---|---|
//! | [`KIND_RECEIPT`] | ZONE REFLECTED, ONE per sender per zone (call C3: coalesced, up to 30 ids) | `delivery-receipt` status 1 | the 1:1 chat |
//! | [`KIND_FS_REJECT`] | an FS envelope with `encapsulated` from a STORED contact (C8) | `FORWARD_SECURITY_ENVELOPE`: `Envelope { session_id, reject { message_id, DISABLED_BY_LOCAL, group_identity } }`, the group identity (P-L4) only when the `Encapsulated` names one | the 1:1 chat |
//! | [`KIND_ANSWER`] x3 | a kept `group-sync-request` of a member, the stored picture EXPLICITLY removed (C7, T-M3) | `group-setup` (the stored members), `group-name`, `group-delete-profile-picture`, each in the creator container | the group, `[sender]` |
//! | [`KIND_EMPTY_SETUP`] | the Common Group Receive Steps (creator) and a request of a non-member or of a left group | `group-setup` with no members (creator container) | the group, `[sender]` |
//! | [`KIND_LEAVE`] | the Common Group Receive Steps (member, the group left) | `group-leave` (member container) | the group, `[sender]` |
//! | [`KIND_SYNC_REQUEST`] | the Common Group Receive Steps (member): the Group Sync Request Steps | `group-sync-request` (member container) | the group, `[creator]` |
//!
//! THE FS `Reject` is written by a hand-made prost struct wire-compatible with `csp-e2e-fs.proto` (`Envelope.session_id`
//! = 1, `Envelope.reject` = 4, `Reject.message_id` = 1 `fixed64`, `Reject.cause` = 2, `Reject.group_identity` = 3), as the steps read the FS envelope
//! (`steps::FsEnvelope`): no `build.rs` change, no generated FS type. C8's LOSS (critic T-L8): an FS message of a sender
//! the file does not hold is discarded and acked WITHOUT a `Reject` (Desktop `incoming-fs-envelope.ts:69-73`): its peer
//! never learns, and the message is lost for every device; `leader::plan` writes one INFO line per such message.
//!
//! THE CREATOR'S ANSWER needs no transaction (C7, Desktop): three jobs, one per message, each with a random id of its own.
//!
//! THE TWO THROTTLES (Desktop's `volatileProtocolState`; in memory per actor, [`Throttles`]; a restart forgets them):
//! the Group Sync Request Steps' "recently resynced for the user" (1 h per group, step 4) and the request's receive steps'
//! "recently resynced for the sender" (1 h per group of ours and sender, step 3). A throttle is set once the zone that
//! wrote its job COMMITTED ([`Throttle`] rides with the zone's entry); a later message of the SAME plan sees it through the
//! plan's own set, so one batch sends one request per group.
//!
//! Pure: no write, no clock but the caller's, no log; `Debug` prints kinds only, never an ID or a body.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use kourier_db::rusqlite::Connection;
use libthreema::common::{MessageId, ThreemaId};
use libthreema::protobuf::common::{CspE2eMessageType as T, GroupIdentity};
use prost::Message as _;

use super::steps::{Job, RejectFs};
use crate::error::{Result, StoreError};
use crate::reflect::body::GroupRef;
use crate::send::NewJob;
use crate::send::body;
use crate::send::prepare::{NewBody, To};
use crate::store;

/// The automatic delivery receipt (status `received`), coalesced per sender and zone.
pub(crate) const KIND_RECEIPT: &str = "receipt";
/// The FS `Reject` of an encapsulated message (C8).
pub(crate) const KIND_FS_REJECT: &str = "fs-reject";
/// One of the creator's three answers to a member's `group-sync-request` (C7).
pub(crate) const KIND_ANSWER: &str = "group-answer";
/// The empty `group-setup` to a sender that is not (or no longer) a member of a group of ours.
pub(crate) const KIND_EMPTY_SETUP: &str = "group-empty-setup";
/// The `group-leave` to a sender of a group the user left.
pub(crate) const KIND_LEAVE: &str = "group-leave";
/// The `group-sync-request` to a group's creator.
pub(crate) const KIND_SYNC_REQUEST: &str = "group-sync-request";

/// Both throttles' span (`csp.struct.yml` Group Sync Request Steps step 4, and the request's receive step 3).
pub(crate) const THROTTLE: Duration = Duration::from_secs(60 * 60);

/// `csp-e2e-fs.proto` `Reject.Cause.DISABLED_BY_LOCAL`.
const DISABLED_BY_LOCAL: i32 = 2;

const OP_ANSWER: &str = "the creator's answer: the group or a member";

/// `csp-e2e-fs.proto` `Envelope` with the `reject` variant only (module doc).
#[derive(Clone, PartialEq, prost::Message)]
struct FsRejectEnvelope {
    #[prost(bytes = "vec", tag = "1")]
    session_id: Vec<u8>,
    #[prost(message, optional, tag = "4")]
    reject: Option<FsReject>,
}

/// `csp-e2e-fs.proto` `Reject`; the group identity only for a rejected GROUP message (P-L4: `:587`; Desktop sends none,
/// `incoming-fs-envelope.ts:75-88`, which is why a group message of its peers is never offered for a re-send).
#[derive(Clone, PartialEq, prost::Message)]
struct FsReject {
    #[prost(fixed64, tag = "1")]
    message_id: u64,
    #[prost(int32, tag = "2")]
    cause: i32,
    #[prost(message, optional, tag = "3")]
    group_identity: Option<GroupIdentity>,
}

fn raw(kind: &'static str, to: To, ty: T, body: Vec<u8>, only: Option<Vec<String>>) -> NewJob {
    NewJob {
        kind,
        to,
        body: NewBody::Raw {
            ty: ty as u8,
            body,
            // Only a type that decides by its content reads it (a receipt as a reaction, an FS envelope): none here does.
            distributes: false,
        },
        only,
    }
}

fn id(identity: &str) -> Result<ThreemaId> {
    ThreemaId::try_from(identity).map_err(|_| StoreError::Failed(OP_ANSWER))
}

fn group_of(group: &GroupRef) -> To {
    To::Group {
        creator: group.creator.clone(),
        group_id: group.group_id,
    }
}

/// The delivery receipt (status `received`) of `ids` to `sender` (call C3: ZONE REFLECTED makes one per sender).
pub(crate) fn receipt(sender: &str, ids: &[u64]) -> NewJob {
    let ids: Vec<MessageId> = ids.iter().copied().map(MessageId).collect();
    raw(
        KIND_RECEIPT,
        To::Contact(sender.to_owned()),
        T::DeliveryReceipt,
        body::delivery_receipt(1, &ids),
        None,
    )
}

/// The FS `Reject { message_id, group_identity, DISABLED_BY_LOCAL }` under the envelope's session to the stored sender
/// (C8; the group identity only when the rejected message is a group message: P-L4).
pub(crate) fn fs_reject(reject: &RejectFs) -> NewJob {
    let envelope = FsRejectEnvelope {
        session_id: reject.session_id.clone(),
        reject: Some(FsReject {
            message_id: reject.message_id.0,
            cause: DISABLED_BY_LOCAL,
            group_identity: reject.group.as_ref().map(|group| GroupIdentity {
                group_id: group.group_id,
                creator_identity: group.creator.clone(),
            }),
        }),
    };
    raw(
        KIND_FS_REJECT,
        To::Contact(reject.to.clone()),
        T::ForwardSecurityEnvelope,
        envelope.encode_to_vec(),
        None,
    )
}

/// The jobs of the steps' `jobs`, in their order (the creator's answer is three). `own` is the user's ID; the
/// creator's answer reads the stored group through `conn` (the members and the name). `Err` by the content: a stored
/// group gone since the steps read it, or a member that is no ID (the caller holds the message).
pub(crate) fn of_steps(conn: &Connection, own: &str, jobs: &[Job]) -> Result<Vec<NewJob>> {
    let mut out = Vec::with_capacity(jobs.len());
    for job in jobs {
        match job {
            Job::SyncRequest { group } => out.push(raw(
                KIND_SYNC_REQUEST,
                group_of(group),
                T::GroupSyncRequest,
                body::group_member_container(id(&group.creator)?, group.group_id, &[]),
                Some(vec![group.creator.clone()]),
            )),
            Job::EmptySetup { to, group_id } => out.push(raw(
                KIND_EMPTY_SETUP,
                To::Group {
                    creator: own.to_owned(),
                    group_id: *group_id,
                },
                T::GroupSetup,
                body::group_creator_container(*group_id, &body::group_setup(&[])),
                Some(vec![to.clone()]),
            )),
            Job::Leave { to, group } => out.push(raw(
                KIND_LEAVE,
                group_of(group),
                T::GroupLeave,
                body::group_member_container(id(&group.creator)?, group.group_id, &[]),
                Some(vec![to.clone()]),
            )),
            Job::Answer { to, group_id } => {
                let group = store::load_group(conn, own, *group_id)?
                    .ok_or(StoreError::Failed(OP_ANSWER))?;
                let members = group
                    .member_identities
                    .as_ref()
                    .map(|m| m.identities.as_slice())
                    .unwrap_or_default()
                    .iter()
                    .filter(|member| member.as_str() != own)
                    .map(|member| id(member))
                    .collect::<Result<Vec<_>>>()?;
                let name = group.name.as_deref().unwrap_or_default();
                let to_group = || To::Group {
                    creator: own.to_owned(),
                    group_id: *group_id,
                };
                let only = || Some(vec![to.clone()]);
                out.push(raw(
                    KIND_ANSWER,
                    to_group(),
                    T::GroupSetup,
                    body::group_creator_container(*group_id, &body::group_setup(&members)),
                    only(),
                ));
                out.push(raw(
                    KIND_ANSWER,
                    to_group(),
                    T::GroupName,
                    body::group_creator_container(*group_id, &body::group_name(name)),
                    only(),
                ));
                out.push(raw(
                    KIND_ANSWER,
                    to_group(),
                    T::GroupDeleteProfilePicture,
                    body::group_creator_container(*group_id, &[]),
                    only(),
                ));
            }
        }
    }
    Ok(out)
}

/// A throttle a committed job sets (module doc).
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum Throttle {
    /// The Group Sync Request Steps' step 4: the group is recently resynced for the user.
    SyncRequest(GroupRef),
    /// The request's receive step 3: the user's group `.0` is recently resynced for the sender `.1`.
    Answer(u64, String),
}

impl fmt::Debug for Throttle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SyncRequest(_) => "Throttle::SyncRequest",
            Self::Answer(..) => "Throttle::Answer",
        })
    }
}

/// The throttles the steps' `jobs` set once their zone committed: every `group-sync-request` sets its group's; the jobs
/// of a kept `group-sync-request` (`answers`: the outer type was one) set the sender's answer, the empty setup too.
pub(crate) fn throttles_of(jobs: &[Job], answers: bool) -> Vec<Throttle> {
    jobs.iter()
        .filter_map(|job| match job {
            Job::SyncRequest { group } => Some(Throttle::SyncRequest(group.clone())),
            Job::Answer { to, group_id } | Job::EmptySetup { to, group_id } if answers => {
                Some(Throttle::Answer(*group_id, to.clone()))
            }
            Job::EmptySetup { .. } | Job::Answer { .. } | Job::Leave { .. } => None,
        })
        .collect()
}

/// The actor's throttles: when each was set (the actor's clock, ms).
#[derive(Default)]
pub(crate) struct Throttles {
    set_at: HashMap<Throttle, u64>,
}

impl Throttles {
    /// Whether `throttle` was set less than [`THROTTLE`] before `now_ms`.
    pub(crate) fn holds(&self, throttle: &Throttle, now_ms: u64) -> bool {
        self.set_at
            .get(throttle)
            .is_some_and(|at| now_ms.saturating_sub(*at) < span_ms())
    }

    /// Sets `throttles` at `now_ms`; the spent ones go (the map stays as small as an hour's jobs).
    pub(crate) fn set(&mut self, throttles: impl IntoIterator<Item = Throttle>, now_ms: u64) {
        self.set_at
            .retain(|_, at| now_ms.saturating_sub(*at) < span_ms());
        for throttle in throttles {
            self.set_at.insert(throttle, now_ms);
        }
    }
}

fn span_ms() -> u64 {
    u64::try_from(THROTTLE.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANN: &str = "PEER0001";

    /// The FS `Reject` is the proto's wire form: field 1 the session id, field 4 the reject with `fixed64` field 1 and
    /// the cause 2 (`DISABLED_BY_LOCAL`) as field 2; nothing else.
    #[test]
    fn the_fs_reject_is_the_protos_wire_form() {
        let job = fs_reject(&RejectFs {
            to: ANN.to_owned(),
            session_id: vec![0xab; 16],
            message_id: MessageId(0x0102_0304_0506_0708),
            group: None,
            ack: true,
        });
        assert_eq!(job.kind, KIND_FS_REJECT);
        assert!(job.only.is_none());
        let NewBody::Raw {
            ty,
            body,
            distributes,
        } = &job.body
        else {
            panic!("not a raw body");
        };
        assert_eq!((*ty, *distributes), (0xa0, false));
        let mut want = vec![0x0a, 16];
        want.extend_from_slice(&[0xab; 16]);
        want.extend_from_slice(&[0x22, 11, 0x09]);
        want.extend_from_slice(&0x0102_0304_0506_0708_u64.to_le_bytes());
        want.extend_from_slice(&[0x10, 2]);
        assert_eq!(body, &want);
    }

    /// P-L4: the `Reject` of a GROUP message carries the group identity as field 3 (`Reject.group_identity`,
    /// `csp-e2e-fs.proto:687`; `GroupIdentity`: `group_id` `fixed64` 1, `creator_identity` string 2), after the cause
    /// (prost writes in tag order); every other byte is the 1:1 form's.
    #[test]
    fn the_fs_reject_of_a_group_message_names_the_group() {
        let job = fs_reject(&RejectFs {
            to: ANN.to_owned(),
            session_id: vec![0xab; 16],
            message_id: MessageId(0x0102_0304_0506_0708),
            group: Some(GroupRef {
                creator: "PEER0002".to_owned(),
                group_id: 0x1112_1314_1516_1718,
            }),
            ack: true,
        });
        let NewBody::Raw { ty, body, .. } = &job.body else {
            panic!("not a raw body");
        };
        assert_eq!(*ty, 0xa0);
        let mut want = vec![0x0a, 16];
        want.extend_from_slice(&[0xab; 16]);
        // Reject: 9 (id) + 2 (cause) + 2 + 19 (the group) = 32 bytes.
        want.extend_from_slice(&[0x22, 32, 0x09]);
        want.extend_from_slice(&0x0102_0304_0506_0708_u64.to_le_bytes());
        want.extend_from_slice(&[0x10, 2]);
        want.extend_from_slice(&[0x1a, 19, 0x09]);
        want.extend_from_slice(&0x1112_1314_1516_1718_u64.to_le_bytes());
        want.extend_from_slice(&[0x12, 8]);
        want.extend_from_slice(b"PEER0002");
        assert_eq!(body, &want);
    }

    /// The throttles: a request's group, a kept request's answer (also its empty setup); the Common Group Receive Steps'
    /// empty setup and leave set none. A throttle holds for an hour less a millisecond, then it is spent and swept.
    #[test]
    fn the_throttles_hold_for_an_hour() {
        let group = GroupRef {
            creator: ANN.to_owned(),
            group_id: 7,
        };
        let jobs = [
            Job::SyncRequest {
                group: group.clone(),
            },
            Job::EmptySetup {
                to: ANN.to_owned(),
                group_id: 9,
            },
            Job::Leave {
                to: ANN.to_owned(),
                group: group.clone(),
            },
        ];
        assert_eq!(
            throttles_of(&jobs, false),
            vec![Throttle::SyncRequest(group.clone())]
        );
        assert_eq!(
            throttles_of(&jobs[1..], true),
            vec![Throttle::Answer(9, ANN.to_owned())]
        );
        let mut throttles = Throttles::default();
        throttles.set(throttles_of(&jobs, false), 1_000);
        let t = Throttle::SyncRequest(group);
        assert!(throttles.holds(&t, 1_000 + span_ms() - 1));
        assert!(!throttles.holds(&t, 1_000 + span_ms()));
        throttles.set([], 1_000 + span_ms());
        assert!(throttles.set_at.is_empty(), "a spent throttle stayed");
    }
}
