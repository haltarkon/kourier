//! A call shown in a chat (Kourier shows calls, it does not make or take them).
//!
//! One line per call: the first signal a backend reports for a call makes the line, every
//! later signal of the same call changes that line. What a line becomes when another signal
//! arrives is [`CallLine::join`], a pure function that is commutative, associative and
//! idempotent, so the signals of one call in any order, any number of times, end in the same
//! line. The order of the states is Kourier's own rule (no reference client has one that is
//! free of the arrival order):
//!
//! ```text
//! started < ringing < missed (incoming) = unanswered (outgoing) < declined < answered
//! ```
//!
//! A line only moves up. `started` is a group call nobody rang this account for; a ring for
//! the same call moves it to `ringing`. A ringing line carries a deadline: past it, with no
//! other signal, the line is missed ([`CallLine::settled`]). A stored line's deadline is on
//! this computer's clock and never further away than [`MAX_RING_MS`]
//! ([`CallLine::on_local_clock`]), whatever the signal said.
//!
//! Who may say what: the other side of a call knows its id and can send any signal for it at
//! any time, so what it says is bounded ([`CallLine::may_come_from_peer`]): an incoming call
//! that was started, rings or was missed. Answered, declined, unanswered and everything of an
//! outgoing call are said by the user's own devices only. Those states are at or above
//! everything the other side may say, so a signal of the other side never moves a line that
//! an own device settled.

use serde::{Deserialize, Serialize};

use crate::ids::UserId;

/// The longest a line rings here (ms), whatever its signal says: two minutes. (RingRTC rings a
/// direct call for 60 s; the id and the deadline of a call are chosen by the other side.)
pub const MAX_RING_MS: i64 = 120_000;

/// Who called whom.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CallDirection {
    /// Somebody called this account (or started a group call).
    Incoming,
    /// This account called, from another of its devices. It wins over `Incoming` when the
    /// signals of one call disagree: only one of our own devices says it.
    Outgoing,
}

/// Voice or video.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CallMedia {
    /// Voice.
    Voice,
    /// Video.
    Video,
}

/// What a call is, or became.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CallState {
    /// A group call was started and nobody rang this account for it.
    Started,
    /// It rings (on the user's phone; Kourier only shows it).
    Ringing {
        /// When it stops ringing by itself (ms since the Unix epoch): past this instant,
        /// with no other signal, the line is missed.
        deadline: i64,
    },
    /// An incoming call nobody answered.
    Missed,
    /// An outgoing call nobody answered.
    Unanswered,
    /// The user declined it on another device (or the callee declined an outgoing call).
    Declined,
    /// It was answered or joined on another device.
    Answered,
}

impl CallState {
    /// The place of the state in the order a line moves up: `Started` 0, `Ringing` 1,
    /// `Missed` and `Unanswered` 2, `Declined` 3, `Answered` 4.
    pub fn rank(self) -> u8 {
        match self {
            Self::Started => 0,
            Self::Ringing { .. } => 1,
            Self::Missed | Self::Unanswered => 2,
            Self::Declined => 3,
            Self::Answered => 4,
        }
    }

    /// The later of two states. Of two ringing states the earlier deadline stays, so a
    /// repeated ring never makes a line ring longer.
    fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Ringing { deadline: a }, Self::Ringing { deadline: b }) => {
                Self::Ringing { deadline: a.min(b) }
            }
            (a, b) if b.rank() > a.rank() => b,
            (a, _) => a,
        }
    }
}

/// A call line, and one signal of a call (a signal is the line it would make alone).
///
/// The two stamps make the join free of the arrival order: `media` is what the LATEST signal
/// that named a kind said, `by` whom the EARLIEST signal that named somebody named.
/// [`CallLine::signal`] sets both to the signal's time.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "CallLineWire")]
pub struct CallLine {
    /// Who called whom.
    pub direction: CallDirection,
    /// A group call.
    pub group: bool,
    /// What it is, or became.
    pub state: CallState,
    /// Voice or video, when a signal said (a hang-up does not; a group call has no kind).
    pub media: Option<CallMedia>,
    /// When the signal that said `media` was made (ms); 0 without `media`.
    pub media_at: i64,
    /// Who rang or started it. `None`: the user himself (`Outgoing`), or nobody is known
    /// (`Incoming`: the phone's sync of a group call names no one).
    pub by: Option<UserId>,
    /// When the signal that named `by` was made (ms); 0 without `by`.
    pub by_at: i64,
}

/// What [`CallLine`] is read from: its own shape, or the `MissedCall { from, video }` notice
/// it replaced (only test data ever held one).
#[derive(Deserialize)]
#[serde(untagged)]
enum CallLineWire {
    Line {
        direction: CallDirection,
        group: bool,
        state: CallState,
        #[serde(default)]
        media: Option<CallMedia>,
        #[serde(default)]
        media_at: i64,
        #[serde(default)]
        by: Option<UserId>,
        #[serde(default)]
        by_at: i64,
    },
    Legacy {
        from: UserId,
        #[serde(default)]
        video: bool,
    },
}

impl From<CallLineWire> for CallLine {
    fn from(w: CallLineWire) -> Self {
        match w {
            CallLineWire::Line {
                direction,
                group,
                state,
                media,
                media_at,
                by,
                by_at,
            } => Self {
                direction,
                group,
                state,
                media,
                media_at,
                by,
                by_at,
            },
            CallLineWire::Legacy { from, video } => Self {
                direction: CallDirection::Incoming,
                group: false,
                state: CallState::Missed,
                media: Some(if video {
                    CallMedia::Video
                } else {
                    CallMedia::Voice
                }),
                media_at: 0,
                by: Some(from),
                by_at: 0,
            },
        }
    }
}

impl CallLine {
    /// Start of the network-local id of a 1:1 call line: `call:<call id>`.
    pub const DIRECT_PREFIX: &'static str = "call:";
    /// Start of the network-local id of a group call line: `gcall:<call id>`.
    pub const GROUP_PREFIX: &'static str = "gcall:";

    /// The network-local id of the line of call `call_id` (`IncomingMessage::local`): every
    /// signal of one call names the same id, whoever sent it.
    pub fn local(group: bool, call_id: &str) -> String {
        let prefix = if group {
            Self::GROUP_PREFIX
        } else {
            Self::DIRECT_PREFIX
        };
        format!("{prefix}{call_id}")
    }

    /// Whether `local` is an id this line may have: the prefix of its kind and a call id.
    pub fn owns_local(&self, local: &str) -> bool {
        let prefix = if self.group {
            Self::GROUP_PREFIX
        } else {
            Self::DIRECT_PREFIX
        };
        local.strip_prefix(prefix).is_some_and(|id| !id.is_empty())
    }

    /// One signal of a call, made at `at` (ms), that names no kind and nobody.
    pub fn signal(direction: CallDirection, group: bool, state: CallState, at: i64) -> Self {
        Self {
            direction,
            group,
            state,
            media: None,
            media_at: at,
            by: None,
            by_at: at,
        }
        .normalized()
    }

    /// The signal, saying voice or video. Call it on a fresh [`CallLine::signal`].
    #[must_use]
    pub fn with_media(mut self, media: CallMedia, at: i64) -> Self {
        self.media = Some(media);
        self.media_at = at;
        self
    }

    /// The signal, naming who rang or started the call. Call it on a fresh
    /// [`CallLine::signal`].
    #[must_use]
    pub fn with_by(mut self, by: UserId, at: i64) -> Self {
        self.by = Some(by);
        self.by_at = at;
        self.normalized()
    }

    /// The one form of a line: an unanswered call is `Missed` when incoming and `Unanswered`
    /// when outgoing; an outgoing call is by the user himself; a stamp without its value is 0.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        if self.state.rank() == CallState::Missed.rank() {
            self.state = match self.direction {
                CallDirection::Incoming => CallState::Missed,
                CallDirection::Outgoing => CallState::Unanswered,
            };
        }
        if self.direction == CallDirection::Outgoing {
            self.by = None;
        }
        if self.by.is_none() {
            self.by_at = 0;
        }
        if self.media.is_none() {
            self.media_at = 0;
        }
        self
    }

    /// What this line becomes when `other`, a signal (or the line) of the same call, arrives.
    ///
    /// Commutative, associative and idempotent. The state is the later one in the order of
    /// [`CallState::rank`] (of two ringing states the earlier deadline); the direction is
    /// `Outgoing` if either says so; the kind is the one of the latest signal that named one;
    /// who rang is whom the earliest signal named.
    #[must_use]
    pub fn join(&self, other: &Self) -> Self {
        let (a, b) = (self.clone().normalized(), other.clone().normalized());
        // Later stamp first; `Video` over `Voice` at the same instant.
        let (media, media_at) = match (a.media, b.media) {
            (Some(x), Some(y)) => {
                if (b.media_at, y) > (a.media_at, x) {
                    (Some(y), b.media_at)
                } else {
                    (Some(x), a.media_at)
                }
            }
            (Some(x), None) => (Some(x), a.media_at),
            (None, y) => (y, b.media_at),
        };
        // Earlier stamp first; the smaller id at the same instant.
        let (by, by_at) = match (a.by, b.by) {
            (Some(x), Some(y)) => {
                if (b.by_at, &y) < (a.by_at, &x) {
                    (Some(y), b.by_at)
                } else {
                    (Some(x), a.by_at)
                }
            }
            (Some(x), None) => (Some(x), a.by_at),
            (None, y) => (y, b.by_at),
        };
        Self {
            direction: a.direction.max(b.direction),
            group: a.group || b.group,
            state: a.state.join(b.state),
            media,
            media_at,
            by,
            by_at,
        }
        .normalized()
    }

    /// The line at `now` (ms): one that rings past its deadline is missed (incoming) or
    /// unanswered (outgoing); every other line is itself.
    #[must_use]
    pub fn settled(&self, now: i64) -> Self {
        let mut line = self.clone().normalized();
        if let CallState::Ringing { deadline } = line.state
            && deadline <= now
        {
            line.state = CallState::Missed;
        }
        line.normalized()
    }

    /// Whether the OTHER side of the call may say this signal: only an incoming call that
    /// was started, rings or was missed. Everything else (answered, declined, unanswered, any
    /// signal of an outgoing call) is said by the user's own devices only.
    pub fn may_come_from_peer(&self) -> bool {
        self.direction == CallDirection::Incoming
            && matches!(
                self.state,
                CallState::Started | CallState::Ringing { .. } | CallState::Missed
            )
    }

    /// The signal as this computer keeps it: a ring's deadline in the terms of the LOCAL
    /// clock and never further away than [`MAX_RING_MS`]. `made_at` is the signal's own time
    /// and `received_at` the local time it was received: the time the ring had left then
    /// (`deadline - made_at`, at least nothing, at most the cap) is added to `received_at`.
    /// A ring with no time left never rings: it is a missed call. Every other signal is
    /// itself.
    #[must_use]
    pub fn on_local_clock(mut self, made_at: i64, received_at: i64) -> Self {
        if let CallState::Ringing { deadline } = self.state {
            let left = deadline.saturating_sub(made_at).clamp(0, MAX_RING_MS);
            self.state = if left == 0 {
                CallState::Missed
            } else {
                CallState::Ringing {
                    deadline: received_at.saturating_add(left),
                }
            };
        }
        self.normalized()
    }

    /// When the line stops ringing by itself, while it rings.
    pub fn deadline(&self) -> Option<i64> {
        match self.state {
            CallState::Ringing { deadline } => Some(deadline),
            _ => None,
        }
    }

    /// Whether the line counts as unread and notifies like a message: an incoming call that
    /// was missed, and a group call somebody started without ringing this account. A ringing
    /// line does not (the phone rings); a declined, answered or outgoing one is read.
    pub fn counts_unread(&self) -> bool {
        self.direction == CallDirection::Incoming
            && matches!(self.state, CallState::Missed | CallState::Started)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(s: &str) -> UserId {
        UserId::parse(s).expect("user id")
    }

    fn ring(at: i64) -> CallLine {
        CallLine::signal(
            CallDirection::Incoming,
            false,
            CallState::Ringing {
                deadline: at + 60_000,
            },
            at,
        )
        .with_media(CallMedia::Voice, at)
        .with_by(uid("aci:a"), at)
    }

    #[test]
    fn a_line_only_moves_up() {
        let answered = CallLine::signal(CallDirection::Incoming, false, CallState::Answered, 9_000)
            .with_media(CallMedia::Video, 9_000);
        let missed = CallLine::signal(CallDirection::Incoming, false, CallState::Missed, 5_000);
        let line = ring(1_000).join(&missed);
        assert_eq!(line.state, CallState::Missed);
        assert_eq!(
            line.media,
            Some(CallMedia::Voice),
            "a hang-up names no kind"
        );
        let line = line.join(&answered);
        assert_eq!(line.state, CallState::Answered);
        assert_eq!(
            line.media,
            Some(CallMedia::Video),
            "the latest signal's kind"
        );
        assert_eq!(line.by, Some(uid("aci:a")));
        // Nothing moves it down again.
        assert_eq!(line.join(&missed).join(&ring(1_000)), line);
        assert!(!line.counts_unread());
    }

    #[test]
    fn a_ring_past_its_deadline_is_missed() {
        let r = ring(1_000);
        assert_eq!(r.settled(60_999), r);
        let missed = r.settled(61_000);
        assert_eq!(missed.state, CallState::Missed);
        assert!(missed.counts_unread() && !r.counts_unread());
        assert_eq!(missed.deadline(), None);
        // A second ring with a later deadline does not make the first one ring longer.
        let longer = ring(1_000).join(&CallLine {
            state: CallState::Ringing { deadline: 900_000 },
            ..ring(2_000)
        });
        assert_eq!(longer.deadline(), Some(61_000));
    }

    #[test]
    fn a_ring_is_kept_on_the_local_clock_and_capped() {
        // 60 s left when it was made, received at 500_000 on this computer's clock.
        let kept = ring(1_000).on_local_clock(1_000, 500_000);
        assert_eq!(kept.deadline(), Some(560_000));
        // A deadline chosen by the other side: at most the cap after the receipt.
        let far = CallLine {
            state: CallState::Ringing { deadline: i64::MAX },
            ..ring(1_000)
        };
        assert_eq!(
            far.on_local_clock(1_000, 500_000).deadline(),
            Some(500_000 + MAX_RING_MS)
        );
        // No time left (or a deadline before the signal): it never rings.
        for deadline in [1_000, 0, i64::MIN] {
            let over = CallLine {
                state: CallState::Ringing { deadline },
                ..ring(1_000)
            };
            let kept = over.on_local_clock(1_000, 500_000);
            assert_eq!(kept.state, CallState::Missed, "{deadline}");
        }
        // The signal's own time is not trusted either.
        let odd = ring(1_000).on_local_clock(i64::MIN, i64::MAX);
        assert_eq!(odd.deadline(), Some(i64::MAX));
        // Every other signal is itself.
        let missed = CallLine::signal(CallDirection::Incoming, false, CallState::Missed, 5);
        assert_eq!(missed.clone().on_local_clock(5, 99), missed);
    }

    #[test]
    fn what_the_other_side_may_say() {
        use CallDirection::{Incoming, Outgoing};
        let says = |d, s| CallLine::signal(d, false, s, 1).may_come_from_peer();
        assert!(says(Incoming, CallState::Started));
        assert!(says(Incoming, CallState::Ringing { deadline: 9 }));
        assert!(says(Incoming, CallState::Missed));
        assert!(!says(Incoming, CallState::Declined));
        assert!(!says(Incoming, CallState::Answered));
        for state in [
            CallState::Started,
            CallState::Ringing { deadline: 9 },
            CallState::Missed,
            CallState::Unanswered,
            CallState::Declined,
            CallState::Answered,
        ] {
            assert!(!says(Outgoing, state), "{state:?}");
        }
    }

    #[test]
    fn direction_decides_between_missed_and_unanswered() {
        let out = CallLine::signal(CallDirection::Outgoing, false, CallState::Missed, 1);
        assert_eq!(out.state, CallState::Unanswered);
        assert!(!out.counts_unread());
        // One of our own devices says the call was ours: nobody else rang.
        let line = ring(1).settled(i64::MAX).join(&out);
        assert_eq!(
            (line.direction, line.state, line.by.clone()),
            (CallDirection::Outgoing, CallState::Unanswered, None)
        );
        assert_eq!(line.by_at, 0);
    }

    #[test]
    fn a_started_group_call_names_its_earliest_caller() {
        let started = |who: &str, at: i64| {
            CallLine::signal(CallDirection::Incoming, true, CallState::Started, at)
                .with_by(uid(who), at)
        };
        let line = started("aci:late", 9).join(&started("aci:first", 3));
        assert_eq!(line.by, Some(uid("aci:first")));
        assert!(line.counts_unread());
        // The phone's sync names nobody and takes nobody away.
        let joined = CallLine::signal(CallDirection::Incoming, true, CallState::Answered, 20);
        let line = joined.join(&line);
        assert_eq!(line.by, Some(uid("aci:first")));
        assert_eq!(line.state, CallState::Answered);
    }

    #[test]
    fn local_ids() {
        assert_eq!(CallLine::local(false, "42"), "call:42");
        assert_eq!(CallLine::local(true, "42"), "gcall:42");
        let line = ring(1);
        assert!(line.owns_local("call:42"));
        assert!(
            !line.owns_local("gcall:42") && !line.owns_local("call:") && !line.owns_local("42")
        );
    }

    #[test]
    fn the_old_missed_call_notice_reads_as_a_missed_line() {
        let old = r#"{"MissedCall":{"from":"aci:a","video":true}}"#;
        let event: crate::SystemEvent = serde_json::from_str(old).expect("old shape");
        let crate::SystemEvent::Call(line) = event else {
            panic!("not a call line");
        };
        assert_eq!(line.state, CallState::Missed);
        assert_eq!(line.media, Some(CallMedia::Video));
        assert_eq!(line.by, Some(uid("aci:a")));
        assert!(!line.group && line.counts_unread());
        // What is written is the new shape, and it reads back as itself.
        let json = serde_json::to_string(&crate::SystemEvent::Call(line.clone())).expect("json");
        assert!(json.starts_with(r#"{"Call":{"#), "{json}");
        assert_eq!(
            serde_json::from_str::<crate::SystemEvent>(&json).ok(),
            Some(crate::SystemEvent::Call(line))
        );
    }
}
