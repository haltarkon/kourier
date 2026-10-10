//! Disappearing-message timers of 1:1 chats (§6.7, §6.13).
//!
//! Ported from Desktop's `SD/ts/models/conversations.preload.ts` `updateExpirationTimer`
//! (4949-5155), `getExpireTimerVersion` / `incrementExpireTimerVersion` (5759-5781),
//! `SD/ts/jobs/helpers/sendDirectExpirationTimerUpdate.preload.ts` and the timer part of
//! `SD/ts/messages/handleDataMessage.preload.ts` (646-683).
//!
//! The chat's timer and its version live in `peers` (`expire_timer`,
//! `expire_timer_version`); every message, edit and reaction Kourier sends carries both
//! ([`crate::send::outgoing`]).
//!
//! **Incoming** ([`incoming`]): a 1:1 `DataMessage` states the timer of its chat, whether it is
//! a timer update (`EXPIRATION_TIMER_UPDATE`) or an ordinary message, from the peer or (in a
//! sync transcript) from another device of ours. Desktop's version rules decide: one with a
//! version below ours is stale; the others raise our version and, when their timer differs
//! from ours, change the chat: one `ConversationUpsert { timer, timer_version }` and the
//! timeline notice `SystemEvent::TimerChanged` (`by` names the peer, `None` is our own
//! account). A timer update's notice has the update's own id (author and timestamp); a change
//! that an ordinary message brings gets the id `timer:<timestamp>` of that message's author
//! and is dated one millisecond earlier, so it stands above the message (Desktop subtracts the
//! same millisecond).
//!
//! A message without a version (0; a client from before timer versions, or a third-party one):
//! - an ordinary message changes nothing (`updateExpirationTimer`: "not updating, zero
//!   version"); it still expires by its own timer;
//! - a timer update changes the chat's timer and leaves our version as it is. Desktop's
//!   `handleDataMessage` sets the chat's timer from every timer update before the version rules
//!   run (they only move the version there), and the phones apply an update without a version
//!   too. Ignoring it would leave Kourier with another timer than the phone it is linked to,
//!   and its next message would silently undo the peer's change.
//!
//! Where Kourier is stricter than Desktop: a timer update with a version *below* ours is
//! ignored (Desktop applies its timer all the same, the phones do not). It is the late copy of
//! a change that was overtaken, which is what versions exist to drop.
//!
//! **Our own change** ([`change`], `Command::ChangeConversation { SetTimer }`): one zone raises
//! the version by one, stores the timer, writes the same two events (the notice with the id the
//! core allocated) and queues the `timer_update` job. The job ([`run_update`]) sends the update
//! to the peer (not urgent, like Desktop) and its sync transcript to our other devices, with
//! retries for a day; in Note to Self it is the transcript alone. The change holds from the
//! zone on, whatever the network does: the next message carries the new timer and version
//! anyway, exactly as Desktop applies its change before its job ran.
//!
//! Groups keep their timer in the group state (`groups::apply` reports it with every state);
//! changing it is a group change, which Kourier cannot write in M1.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{AllocatedId, BackendEvent, IncomingMessage};
use kourier_db::Tx;
use kourier_db::rusqlite::Connection;
use kourier_model::{
    BackendError, Body, ConvKey, ConversationPatch, ErrorKind, Origin, Patch, SystemEvent, UserId,
};
use libsignal_core::ServiceId;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::actor::Shared;
use crate::db::Account;
use crate::error::{Error, Result};
use crate::features::{self, MAX_TIMER_SECS};
use crate::jobs::{self, Done, Job, JobKind, is_permanent};
use crate::net::SignalTransport;
use crate::outbox::append_events;
use crate::peers;
use crate::send::content::{self, RecipientStatus, Transcript, Wire};
use crate::send::{self, Delivery, Outgoing};
use crate::service_id::{SignalUser, parse_user_id};

/// Desktop's `INITIAL_EXPIRE_TIMER_VERSION`: the version of a chat nobody changed yet.
const INITIAL_VERSION: u32 = 1;
/// How long the update of our own change is retried (Desktop's conversation job queue: a day).
const UPDATE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Who a timer came from, and under which id its notice is filed.
#[derive(Clone, Debug)]
pub(crate) struct Source {
    /// The origin of the message that stated the timer.
    pub(crate) origin: Origin,
    /// `by` of the notice: the peer, or `None` for our own account.
    pub(crate) by: Option<UserId>,
    /// The notice's network id.
    pub(crate) local: String,
    /// The notice's sent time (ms).
    pub(crate) sent_at: i64,
    /// When the message arrived (ms).
    pub(crate) received_at: i64,
    /// Its server timestamp.
    pub(crate) server_at: Option<i64>,
    /// Its arrival counter.
    pub(crate) arrival: u64,
    /// It arrived before the catch-up boundary.
    pub(crate) catch_up: bool,
}

/// A timer in seconds as the model has it (0 = off).
fn duration(secs: u32) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(u64::from(secs)))
}

/// The two events of a timer change in `conv`: the patch and its timeline notice.
fn events(conv: &ConvKey, timer: u32, version: u32, notice: IncomingMessage) -> Vec<BackendEvent> {
    let mut patch = ConversationPatch::new(conv.clone());
    patch.timer = duration(timer).map_or(Patch::Clear, Patch::Set);
    patch.timer_version = Some(version);
    vec![
        BackendEvent::ConversationUpsert(patch),
        BackendEvent::Message(notice),
    ]
}

/// The timer a 1:1 message states for its chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stated {
    /// The timer in seconds, 0 = off.
    pub(crate) timer: u32,
    /// Its version, 0 when the message has none.
    pub(crate) version: u32,
    /// The message is a timer update (`EXPIRATION_TIMER_UPDATE`), not an ordinary message that
    /// carries its chat's timer.
    pub(crate) update: bool,
}

/// A 1:1 message from `source` in the chat with `peer` (a service id string) stated a timer.
/// Applies the version rules on `conn` (module docs) and returns the events of a change; empty
/// when nothing changed.
pub(crate) fn incoming(
    conn: &Connection,
    peer: &str,
    conv: &ConvKey,
    stated: Stated,
    source: Source,
    now: i64,
) -> Result<Vec<BackendEvent>> {
    let row = peers::load(conn, peer)?;
    let ours = row.expire_timer_version.max(INITIAL_VERSION);
    let version = match stated.version {
        // A timer update without a version: the timer applies, our version stays.
        0 if stated.update => ours,
        // `updateExpirationTimer`: "not updating, zero version".
        0 => return Ok(Vec::new()),
        version if version < ours => return Ok(Vec::new()),
        version => version,
    };
    let timer = stated.timer;
    if timer == row.expire_timer {
        if version > row.expire_timer_version {
            peers::raise_timer_version(conn, peer, version, now)?;
        }
        return Ok(Vec::new());
    }
    peers::set_expire_timer(conn, peer, timer, version, now)?;
    let mut notice = IncomingMessage::new(
        conv.clone(),
        source.local,
        source.origin,
        Body::System(SystemEvent::TimerChanged {
            by: source.by,
            timer: duration(timer),
        }),
        source.sent_at,
        source.received_at,
    );
    notice.server_at = source.server_at;
    notice.arrival = Some(source.arrival);
    notice.catch_up = source.catch_up;
    Ok(events(conv, timer, version, notice))
}

/// Why [`change`] did not happen: nothing of it is stored either way.
#[derive(Debug)]
pub(crate) enum ChangeError {
    /// The chat does not take this change.
    Refused(BackendError),
    /// `backend.db` failed.
    Failed(Error),
}

impl ChangeError {
    /// The answer for the core.
    pub(crate) fn into_backend(self) -> BackendError {
        match self {
            Self::Refused(error) => error,
            Self::Failed(error) => error.to_backend_error(),
        }
    }
}

impl From<Error> for ChangeError {
    fn from(error: Error) -> Self {
        Self::Failed(error)
    }
}

impl From<kourier_db::Error> for ChangeError {
    fn from(error: kourier_db::Error) -> Self {
        Self::Failed(error.into())
    }
}

/// What a `timer_update` job sends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Update {
    /// The peer (service id string of an ACI).
    peer: String,
    /// The update's sent timestamp (the id the core allocated).
    timestamp: u64,
    /// The timer in seconds, 0 = off.
    timer: u32,
    /// The version this change has.
    version: u32,
}

/// `Command::ChangeConversation { SetTimer }` for the 1:1 chat with `user`: the change in one
/// zone (module docs). `Ok(true)`: a job was queued.
pub(crate) fn change(
    tx: &mut Tx<'_>,
    account: &Account,
    user: &UserId,
    timer: Option<Duration>,
    id: Option<&AllocatedId>,
    now: i64,
    wake: &Rc<Notify>,
) -> std::result::Result<bool, ChangeError> {
    let invalid =
        |what: &'static str| ChangeError::Refused(BackendError::new(ErrorKind::InvalidInput, what));
    let aci = match parse_user_id(user) {
        Ok(SignalUser::Aci(aci)) => aci,
        _ => return Err(invalid("timers need a chat with a Signal account")),
    };
    let secs = match timer {
        None => 0,
        Some(d) => u32::try_from(d.as_secs())
            .ok()
            // The upper bound is what Android reads as it was sent ([`MAX_TIMER_SECS`]).
            .filter(|s| *s > 0 && *s <= MAX_TIMER_SECS && d.subsec_nanos() == 0)
            .ok_or_else(|| {
                invalid("a timer is a whole number of seconds from 1 to 2,147,483,647")
            })?,
    };
    let peer = ServiceId::from(aci);
    let sid = peer.service_id_string();
    let row = peers::load(tx, &sid)?;
    if row.blocked {
        return Err(ChangeError::Refused(BackendError::unsupported(
            "the chat is blocked",
        )));
    }
    if !features::is_accepted(tx, account, peer)? {
        // The update carries our profile key, like a message (§6.7 M1 rule).
        return Err(ChangeError::Refused(
            BackendError::new(ErrorKind::Unsupported, "chat not accepted on the phone")
                .with_user_message("Start this chat on your phone first."),
        ));
    }
    let conv = ConvKey::Direct(user.clone());
    let current = row.expire_timer_version.max(INITIAL_VERSION);
    if secs == row.expire_timer {
        // Nothing to change (`timerMatchesLocalValue`); the core hears what holds.
        let mut patch = ConversationPatch::new(conv);
        patch.timer = duration(secs).map_or(Patch::Clear, Patch::Set);
        patch.timer_version = Some(current);
        append_events(tx, &[BackendEvent::ConversationUpsert(patch)], now, wake)?;
        return Ok(false);
    }
    let id = id.ok_or_else(|| invalid("a timer change needs a message id"))?;
    let timestamp = u64::try_from(id.sent_at).map_err(|_| invalid("negative sent_at"))?;
    // `incrementExpireTimerVersion`: one above ours, never past the proto's range.
    let version = current.saturating_add(1);
    peers::set_expire_timer(tx, &sid, secs, version, now)?;
    let notice = IncomingMessage::new(
        conv.clone(),
        id.local.to_string(),
        Origin::OwnThisDevice,
        Body::System(SystemEvent::TimerChanged {
            by: None,
            timer: duration(secs),
        }),
        id.sent_at,
        now,
    );
    append_events(tx, &events(&conv, secs, version, notice), now, wake)?;
    let update = Update {
        peer: sid.clone(),
        timestamp,
        timer: secs,
        version,
    };
    let payload =
        serde_json::to_vec(&update).map_err(|_| Error::Internal("timer update payload"))?;
    jobs::enqueue_payload(
        tx,
        JobKind::TimerUpdate,
        &format!("{sid}:{timestamp}"),
        &payload,
        now,
        Some(UPDATE_MAX_AGE),
    )?;
    Ok(true)
}

/// Runs a `timer_update` job: the update to the peer, then its sync transcript (module docs).
/// A peer that can never get it (unregistered) is skipped; our devices still hear of it.
pub(crate) async fn run_update<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    job: &Job,
) -> Result<Done> {
    let Some(update) = job
        .payload
        .as_deref()
        .and_then(|p| serde_json::from_slice::<Update>(p).ok())
    else {
        tracing::warn!("timer update job without a valid payload dropped");
        return Ok(Done::Finished);
    };
    let Some(ServiceId::Aci(aci)) = ServiceId::parse_from_service_id_string(&update.peer) else {
        tracing::warn!("timer update job for a non-ACI peer dropped");
        return Ok(Done::Finished);
    };
    let account = shared.account().ok_or(Error::NotLinked)?;
    let ctx = shared.send_context(&account, socket);
    let wire = Wire::Data(content::timer_update_message(
        update.timestamp,
        &account.profile_key,
        update.timer,
        update.version,
    ));
    let local = update.timestamp.to_string();
    let peer = ServiceId::from(aci);
    let mut statuses = Vec::new();
    if account.aci != aci {
        let share = shared
            .db
            .read(|c| peers::load(c, &update.peer))?
            .share_my_number;
        let mut outgoing = wire.content();
        if share {
            content::add_pni_signature(&mut outgoing, &account)?;
        }
        let sent = send::send_to(
            &ctx,
            peer,
            &Outgoing {
                content: &outgoing,
                timestamp: update.timestamp,
                delivery: Delivery::TIMER_UPDATE,
                local: Some(&local),
                group: None,
            },
        )
        .await;
        match sent {
            Ok(accepted) => statuses.push(RecipientStatus {
                recipient: peer,
                sealed: accepted.sealed,
            }),
            Err(e) if is_permanent(&e) => {
                tracing::info!(error = %e, "timer update to the peer dropped");
            }
            Err(e) => return Err(e),
        }
    }
    let transcript = content::sent_transcript(&Transcript {
        sent: &wire,
        timestamp: update.timestamp,
        destination: Some(peer),
        statuses: &statuses,
        expiration_start: None,
        is_recipient_update: false,
    });
    send::send_to(
        &ctx,
        account.aci.into(),
        &Outgoing {
            content: &transcript,
            timestamp: update.timestamp,
            delivery: Delivery::TRANSCRIPT,
            local: Some(&local),
            group: None,
        },
    )
    .await?;
    tracing::debug!(version = update.version, "timer update sent");
    Ok(Done::Finished)
}

#[cfg(test)]
mod tests {
    use kourier_backend_api::decode_outbox_row;
    use libsignal_core::Aci;
    use uuid::Uuid;

    use super::*;
    use crate::receive::tests::fixture;
    use crate::service_id::aci_user_id;

    fn source(user: &UserId, local: &str, sent_at: i64) -> Source {
        Source {
            origin: Origin::Remote {
                author: user.clone(),
                device: Some(1),
            },
            by: Some(user.clone()),
            local: local.to_owned(),
            sent_at,
            received_at: sent_at,
            server_at: None,
            arrival: 1,
            catch_up: false,
        }
    }

    fn timer_of(events: &[BackendEvent]) -> Option<(Option<Duration>, Option<u32>)> {
        events.iter().find_map(|e| match e {
            BackendEvent::ConversationUpsert(patch) => Some((
                match &patch.timer {
                    Patch::Set(d) => Some(*d),
                    _ => None,
                },
                patch.timer_version,
            )),
            _ => None,
        })
    }

    /// Desktop's `updateExpirationTimer` for a timer set by someone else: an older version is
    /// stale, the same version with another timer still applies ("glare"), the same timer only
    /// raises the version; without a version an ordinary message changes nothing and a timer
    /// update changes the timer alone.
    #[test]
    fn incoming_timers_follow_the_version_rules() {
        let fixture = fixture();
        let alice = Aci::from(Uuid::from_u128(0xa11ce));
        let user = aci_user_id(alice).unwrap();
        let sid = ServiceId::from(alice).service_id_string();
        let conv = ConvKey::Direct(user.clone());
        fixture
            .db
            .zone(|tx| -> Result<()> {
                let state = |timer: u32, version: u32, update: bool, at: i64| {
                    incoming(
                        tx,
                        &sid,
                        &conv,
                        Stated {
                            timer,
                            version,
                            update,
                        },
                        source(&user, &at.to_string(), at),
                        at,
                    )
                };
                let apply = |timer: u32, version: u32, at: i64| state(timer, version, true, at);
                assert!(
                    state(60, 0, false, 1)?.is_empty(),
                    "an ordinary message without a version: ignored"
                );
                assert_eq!(peers::load(tx, &sid)?.expire_timer, 0);

                let events = apply(60, 2, 2)?;
                assert_eq!(events.len(), 2, "the patch and its notice");
                assert_eq!(
                    timer_of(&events),
                    Some((Some(Duration::from_secs(60)), Some(2)))
                );
                let BackendEvent::Message(notice) = &events[1] else {
                    panic!("notice");
                };
                assert_eq!(&*notice.local, "2");
                assert_eq!(
                    notice.body,
                    Body::System(SystemEvent::TimerChanged {
                        by: Some(user.clone()),
                        timer: Some(Duration::from_secs(60)),
                    })
                );
                let row = peers::load(tx, &sid)?;
                assert_eq!((row.expire_timer, row.expire_timer_version), (60, 2));

                assert!(apply(30, 1, 3)?.is_empty(), "an older version is stale");
                assert_eq!(peers::load(tx, &sid)?.expire_timer, 60);

                // The same timer with a newer version: no change to show, the version counts.
                assert!(apply(60, 5, 4)?.is_empty());
                assert_eq!(peers::load(tx, &sid)?.expire_timer_version, 5);

                // The same version, another timer (both sides changed it at once): it applies.
                let events = apply(0, 5, 5)?;
                assert_eq!(timer_of(&events), Some((None, Some(5))), "off");
                assert_eq!(peers::load(tx, &sid)?.expire_timer, 0);

                // A timer update without a version (a client from before versions): the timer
                // applies as on Desktop and the phones, our version stays where it was.
                let events = apply(90, 0, 6)?;
                assert_eq!(
                    timer_of(&events),
                    Some((Some(Duration::from_secs(90)), Some(5)))
                );
                let row = peers::load(tx, &sid)?;
                assert_eq!((row.expire_timer, row.expire_timer_version), (90, 5));
                assert!(
                    apply(90, 0, 7)?.is_empty(),
                    "the same timer: nothing to show"
                );
                // The same from an ordinary message changes nothing, and a stale update is
                // still ignored.
                assert!(state(0, 0, false, 8)?.is_empty());
                assert!(apply(0, 4, 9)?.is_empty(), "stale");
                assert_eq!(peers::load(tx, &sid)?.expire_timer, 90);
                Ok(())
            })
            .unwrap();
    }

    /// Our own change: the version goes up by one from Desktop's first version, the notice has
    /// the id the core allocated and no author, the job carries what to send; the same timer
    /// again changes nothing; a chat the phone has not accepted refuses.
    #[test]
    fn our_change_raises_the_version_and_queues_the_update() {
        let fixture = fixture();
        let account = fixture.account.clone();
        let alice = Aci::from(Uuid::from_u128(0xa11ce));
        let user = aci_user_id(alice).unwrap();
        let sid = ServiceId::from(alice).service_id_string();
        let wake = Rc::new(Notify::new());
        let id = AllocatedId {
            local: "7000".into(),
            sent_at: 7000,
        };
        let outbox = |tx: &Tx<'_>| -> Vec<BackendEvent> {
            let mut stmt = tx
                .prepare("SELECT envelope FROM core_outbox ORDER BY seq")
                .unwrap();
            stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))
                .unwrap()
                .flat_map(|row| decode_outbox_row(&row.unwrap()).unwrap())
                .collect()
        };
        fixture
            .db
            .zone(|tx| -> Result<()> {
                let timer = Some(Duration::from_secs(30));
                let refused = change(tx, &account, &user, timer, Some(&id), 10, &wake)
                    .unwrap_err()
                    .into_backend();
                assert_eq!(refused.kind, ErrorKind::Unsupported, "not accepted yet");
                assert!(outbox(tx).is_empty());

                peers::set_profile_sharing(tx, &sid, true, 11)?;
                assert!(change(tx, &account, &user, timer, Some(&id), 12, &wake).unwrap());
                let row = peers::load(tx, &sid)?;
                assert_eq!((row.expire_timer, row.expire_timer_version), (30, 2));
                let events = outbox(tx);
                assert_eq!(
                    timer_of(&events),
                    Some((Some(Duration::from_secs(30)), Some(2)))
                );
                let Some(BackendEvent::Message(notice)) = events.last() else {
                    panic!("notice last: {events:?}");
                };
                assert_eq!((&*notice.local, notice.sent_at), ("7000", 7000));
                assert_eq!(notice.origin, Origin::OwnThisDevice);
                assert_eq!(
                    notice.body,
                    Body::System(SystemEvent::TimerChanged {
                        by: None,
                        timer: Some(Duration::from_secs(30)),
                    })
                );
                let job = jobs::next_due(tx, 12, true, false)?.expect("the update job");
                assert_eq!(job.kind, JobKind::TimerUpdate);
                let update: Update =
                    serde_json::from_slice(job.payload.as_deref().unwrap()).unwrap();
                assert_eq!(
                    update,
                    Update {
                        peer: sid.clone(),
                        timestamp: 7000,
                        timer: 30,
                        version: 2,
                    }
                );

                // The same timer again: nothing is sent, the version stays.
                let before = jobs::count(tx, JobKind::TimerUpdate)?;
                assert!(!change(tx, &account, &user, timer, Some(&id), 13, &wake).unwrap());
                assert_eq!(jobs::count(tx, JobKind::TimerUpdate)?, before);
                assert_eq!(peers::load(tx, &sid)?.expire_timer_version, 2);

                // Off is version 3; a timer below a second, or above what Android reads as it
                // was sent (a signed 32-bit number of seconds), is refused.
                let off = AllocatedId {
                    local: "8000".into(),
                    sent_at: 8000,
                };
                assert!(change(tx, &account, &user, None, Some(&off), 14, &wake).unwrap());
                let row = peers::load(tx, &sid)?;
                assert_eq!((row.expire_timer, row.expire_timer_version), (0, 3));
                for bad in [
                    Duration::from_millis(500),
                    Duration::from_millis(60_500),
                    Duration::from_secs(2_147_483_648),
                    Duration::from_secs(u64::from(u32::MAX)),
                    Duration::from_secs(u64::from(u32::MAX) + 1),
                ] {
                    let refused = change(tx, &account, &user, Some(bad), Some(&id), 15, &wake)
                        .unwrap_err()
                        .into_backend();
                    assert_eq!(refused.kind, ErrorKind::InvalidInput);
                }
                // Four weeks is where the official pickers end, not a limit: ten weeks and
                // the largest value of all are taken like any other timer.
                let ten_weeks = 10 * 7 * 24 * 3600;
                for (n, secs) in [(0, ten_weeks), (1, 2_147_483_647)] {
                    let id = AllocatedId {
                        local: format!("{}", 9000 + n).into(),
                        sent_at: 9000 + n,
                    };
                    let timer = Some(Duration::from_secs(u64::from(secs)));
                    assert!(change(tx, &account, &user, timer, Some(&id), 16, &wake).unwrap());
                    assert_eq!(peers::load(tx, &sid)?.expire_timer, secs);
                }
                Ok(())
            })
            .unwrap();
    }
}
