//! Calls on the fake network: one function per signal a linked device gets for a call it does
//! not take, each as the event a backend reports it with (`IncomingMessage::call`), and
//! [`script`], an account that receives every one of them.
//!
//! The signals are those of Signal, in Kourier's contract. The other side of a call says only
//! that an incoming call was started, rings or was missed; answered, declined, unanswered and
//! every signal of an outgoing call come from the user's own devices:
//!
//! | Signal on the wire | From | Function | The line it makes or joins |
//! |---|---|---|---|
//! | 1:1 Offer | the caller | [`Call::ring`] | incoming, ringing for [`RING_MS`] from its receipt, voice or video, by the caller |
//! | the caller's Hangup, of any type | the caller | [`Call::caller_gave_up`] | incoming, missed (names no kind) |
//! | sync INCOMING ACCEPTED | own device | [`Call::answered_elsewhere`] | incoming, answered |
//! | sync INCOMING NOT_ACCEPTED | own device | [`Call::declined`] | incoming, declined |
//! | sync OUTGOING ACCEPTED | own device | [`Call::outgoing_answered`] | outgoing, answered |
//! | sync OUTGOING NOT_ACCEPTED | own device | [`Call::outgoing_unanswered`] | outgoing, unanswered |
//! | sync DELETE | own device | [`Call::deleted`] | the line goes |
//! | group ring RING | the ringer | [`Call::ring`] of a group call | incoming, ringing, by the ringer |
//! | group ring CANCELLED | the ringer | [`Call::ring_cancelled`] | incoming, missed |
//! | groupCallUpdate | its sender | [`Call::started`] | incoming, started, by its sender |
//! | sync GROUP ACCEPTED | own device | [`Call::joined_elsewhere`] | incoming, answered (names nobody) |
//! | sync GROUP NOT_ACCEPTED | own device | [`Call::declined`] of a group call | incoming, declined (names nobody) |

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_model::{CallDirection, CallLine, CallMedia, CallState, ConvKey, Origin, UserId};

use crate::script::{Script, ScriptError, Step};
use crate::scripts::T0;

/// How long a call rings on the fake network before it counts as missed (RingRTC rings a
/// direct call for 60 s; a backend adds its margin).
pub const RING_MS: i64 = 65_000;

/// How long after a signal was made the fake's stage 1 stored it (as `scripts::remote`).
pub const DELIVERY_MS: i64 = 500;

/// When a ring made at `at` ends on the local clock: [`RING_MS`] after its receipt (the core
/// keeps a ring's deadline in the terms of its own clock).
pub fn ring_end(at: i64) -> i64 {
    at + DELIVERY_MS + RING_MS
}

/// One call as the fake network knows it: where its line is shown and its id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// The chat: the peer's, or the group's.
    pub conv: ConvKey,
    /// The network's id of the call (the same in every signal).
    pub id: String,
    /// Who calls (1:1: the peer) or rings the group.
    pub caller: UserId,
    /// Voice or video (a group call names none).
    pub media: Option<CallMedia>,
}

impl Call {
    /// A 1:1 call with `peer`.
    pub fn direct(peer: &UserId, id: u64, media: CallMedia) -> Self {
        Self {
            conv: ConvKey::Direct(peer.clone()),
            id: id.to_string(),
            caller: peer.clone(),
            media: Some(media),
        }
    }

    /// A call in `group` that `ringer` rings (or starts).
    pub fn group(group: &ConvKey, id: u64, ringer: &UserId) -> Self {
        Self {
            conv: group.clone(),
            id: id.to_string(),
            caller: ringer.clone(),
            media: None,
        }
    }

    fn is_group(&self) -> bool {
        !matches!(self.conv, ConvKey::Direct(_))
    }

    /// The other side of the call: who rings.
    fn peer(&self) -> Origin {
        Origin::Remote {
            author: self.caller.clone(),
            device: None,
        }
    }

    /// One signal of this call made at `at` and said by `origin`: `named` says whether it
    /// names the caller and `kind` whether it says voice or video.
    fn signal(
        &self,
        origin: Origin,
        direction: CallDirection,
        state: CallState,
        at: i64,
        named: bool,
        kind: bool,
    ) -> IncomingMessage {
        let mut line = CallLine::signal(direction, self.is_group(), state, at);
        if let (true, Some(media)) = (kind, self.media) {
            line = line.with_media(media, at);
        }
        if named {
            line = line.with_by(self.caller.clone(), at);
        }
        IncomingMessage::call(
            self.conv.clone(),
            &self.id,
            origin,
            line,
            at,
            at + DELIVERY_MS,
        )
    }

    /// A signal of the other side of the call.
    fn said_by_peer(&self, state: CallState, at: i64, named: bool, kind: bool) -> BackendEvent {
        BackendEvent::Message(self.signal(
            self.peer(),
            CallDirection::Incoming,
            state,
            at,
            named,
            kind,
        ))
    }

    /// A signal of one of the user's own devices (the phone's sync).
    fn synced(
        &self,
        direction: CallDirection,
        state: CallState,
        at: i64,
        named: bool,
        kind: bool,
    ) -> BackendEvent {
        let own = Origin::OwnOtherDevice { device: None };
        BackendEvent::Message(self.signal(own, direction, state, at, named, kind))
    }

    /// The call rings: the signal is made at `at` and says the ring ends at `deadline` (the
    /// core keeps `deadline - at`, capped, from the signal's receipt).
    pub fn ring_until(&self, at: i64, deadline: i64) -> BackendEvent {
        self.said_by_peer(CallState::Ringing { deadline }, at, true, true)
    }

    /// The call rings from `at` for [`RING_MS`] (a 1:1 Offer, a group ring).
    pub fn ring(&self, at: i64) -> BackendEvent {
        self.ring_until(at, at + RING_MS)
    }

    /// The ring of [`Call::ring`] as a message, to change before it is delivered (`catch_up`,
    /// `silent`).
    pub fn ring_message(&self, at: i64) -> IncomingMessage {
        self.signal(
            self.peer(),
            CallDirection::Incoming,
            CallState::Ringing {
                deadline: at + RING_MS,
            },
            at,
            true,
            true,
        )
    }

    /// The caller of a 1:1 call hung up before anybody answered here (a Hangup of any type:
    /// the caller's side ended it). A hang-up names no kind.
    pub fn caller_gave_up(&self, at: i64) -> BackendEvent {
        self.said_by_peer(CallState::Missed, at, true, false)
    }

    /// The ringer of a group call took the ring back.
    pub fn ring_cancelled(&self, at: i64) -> BackendEvent {
        self.said_by_peer(CallState::Missed, at, true, false)
    }

    /// The user answered the 1:1 call on another device (the phone's sync).
    pub fn answered_elsewhere(&self, at: i64) -> BackendEvent {
        let named = !self.is_group();
        self.synced(
            CallDirection::Incoming,
            CallState::Answered,
            at,
            named,
            true,
        )
    }

    /// The user joined the group call on another device. The sync names the group and
    /// nobody else.
    pub fn joined_elsewhere(&self, at: i64) -> BackendEvent {
        self.synced(
            CallDirection::Incoming,
            CallState::Answered,
            at,
            false,
            false,
        )
    }

    /// The user declined the call on another device (a group's sync names nobody).
    pub fn declined(&self, at: i64) -> BackendEvent {
        let named = !self.is_group();
        self.synced(
            CallDirection::Incoming,
            CallState::Declined,
            at,
            named,
            true,
        )
    }

    /// Somebody started (or joined) the group call without ringing us: `by` sent the
    /// group-call update.
    pub fn started(&self, by: &UserId, at: i64) -> BackendEvent {
        let line = CallLine::signal(CallDirection::Incoming, true, CallState::Started, at)
            .with_by(by.clone(), at);
        BackendEvent::Message(IncomingMessage::call(
            self.conv.clone(),
            &self.id,
            Origin::Remote {
                author: by.clone(),
                device: None,
            },
            line,
            at,
            at + DELIVERY_MS,
        ))
    }

    /// A call the user made from another device was answered.
    pub fn outgoing_answered(&self, at: i64) -> BackendEvent {
        self.synced(
            CallDirection::Outgoing,
            CallState::Answered,
            at,
            false,
            true,
        )
    }

    /// A call the user made from another device ended unanswered.
    pub fn outgoing_unanswered(&self, at: i64) -> BackendEvent {
        self.synced(
            CallDirection::Outgoing,
            CallState::Unanswered,
            at,
            false,
            true,
        )
    }

    /// The user deleted the call's entry on another device.
    pub fn deleted(&self, at: i64) -> BackendEvent {
        BackendEvent::Deleted {
            target: IncomingMessage::call_target(self.conv.clone(), self.is_group(), &self.id),
            by: None,
            for_everyone: false,
            at,
        }
    }

    /// What the other side of the call must never get through: the signal `state` of an
    /// incoming or outgoing call, said by the caller (a backend that reports it breaks the
    /// contract).
    pub fn forged_by_peer(
        &self,
        direction: CallDirection,
        state: CallState,
        at: i64,
    ) -> IncomingMessage {
        self.signal(self.peer(), direction, state, at, true, true)
    }

    /// The `local` of the call's line.
    pub fn local(&self) -> String {
        CallLine::local(self.is_group(), &self.id)
    }
}

/// The people and chats of [`script`].
#[derive(Clone, Debug)]
pub struct Cast {
    /// Our account.
    pub me: UserId,
    /// A contact.
    pub alice: UserId,
    /// Another contact.
    pub bob: UserId,
    /// A group with both.
    pub group: ConvKey,
}

/// The cast of [`script`].
pub fn cast() -> Result<Cast, ScriptError> {
    Ok(Cast {
        me: UserId::parse("aci:00000000-0000-4000-8000-0000000000aa")?,
        alice: UserId::parse("aci:00000000-0000-4000-8000-0000000a11ce")?,
        bob: UserId::parse("aci:00000000-0000-4000-8000-000000000b0b")?,
        group: ConvKey::Group("gv2:ZmFrZS1jYWxsLWdyb3Vw".into()),
    })
}

/// A Signal-like account that gets every signal of the design's table, one delivery each, so
/// a permutation of its deliveries is a permutation of the signals: a 1:1 call that rings and
/// is given up, one answered on the phone, one declined there, one that only ever rings, two
/// made from the phone, one whose entry the phone deletes, and in a group a ring that is
/// cancelled, a ring answered on the phone, a ring declined there and a call that was only
/// started (twice, by two members).
pub fn script() -> Result<Script, ScriptError> {
    let c = cast()?;
    let ts = |n: i64| T0 + n * 1000;
    let voice = |id: u64| Call::direct(&c.alice, id, CallMedia::Voice);
    let video = |id: u64| Call::direct(&c.bob, id, CallMedia::Video);
    let group = |id: u64| Call::group(&c.group, id, &c.alice);
    let (given_up, answered, declined, rings) = (voice(101), video(102), voice(103), video(104));
    let (out_ok, out_none, removed) = (voice(105), video(106), voice(107));
    let (g_cancel, g_join, g_decline, g_started) = (group(201), group(202), group(203), group(204));
    let s = Script::new("calls", vec![c.me.clone()])
        .step(Step::Conn(kourier_model::ConnState::Syncing {
            fraction: None,
        }))
        .step(Step::CatchUpDone)
        .deliver(vec![given_up.ring(ts(10))])
        .deliver(vec![given_up.caller_gave_up(ts(25))])
        .deliver(vec![answered.ring(ts(40))])
        .deliver(vec![answered.answered_elsewhere(ts(48))])
        .deliver(vec![declined.ring(ts(60))])
        .deliver(vec![declined.declined(ts(63))])
        .deliver(vec![rings.ring(ts(80))])
        .deliver(vec![out_ok.outgoing_answered(ts(100))])
        .deliver(vec![out_none.outgoing_unanswered(ts(110))])
        .deliver(vec![removed.ring(ts(120))])
        .deliver(vec![removed.caller_gave_up(ts(130))])
        .deliver(vec![removed.deleted(ts(140))])
        .deliver(vec![g_cancel.ring(ts(200))])
        .deliver(vec![g_cancel.ring_cancelled(ts(220))])
        .deliver(vec![g_join.ring(ts(240))])
        .deliver(vec![g_join.joined_elsewhere(ts(250))])
        .deliver(vec![g_decline.ring(ts(260))])
        .deliver(vec![g_decline.declined(ts(265))])
        .deliver(vec![g_started.started(&c.bob, ts(280))])
        .deliver(vec![g_started.started(&c.alice, ts(290))]);
    Ok(s)
}
