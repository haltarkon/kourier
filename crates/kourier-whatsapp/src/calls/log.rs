//! The phone's call log as call lines (W6a1, chunk A6; decisions D12 and H2 of `target/wa/w6-decisions.md`, his
//! answer "Show them Quietly but it should be in list of chats", critic L6).
//!
//! WHAT IT IS. A call the phone placed puts nothing on this socket, and a call that rang while Kourier was off may
//! have left no offer behind; the phone's call log (the app-state verb `call_log` of the `regular` collection) says
//! what became of every call. Each record becomes ONE signal of the call's line, in the conversation and under the id
//! the interceptor's line used, so the log never makes a second line for a call the interceptor saw.
//!
//! TWO SOURCES, one map: the app-state mutation (`Seen::from_sync`, the handler's zone) and the history push's
//! `callLogRecords` (`Seen::from_record`, kept in `history_deferred` kind 1 and taken up once the push is over:
//! `crate::history::deliver::take_up_calls`, D13). A history record has no mutation, so it needs its own id,
//! creator and start; one that lacks them gives no line.
//!
//! THE FIELDS (upstream `CallLogSync`, scout A1): the creator and the call id come from the mutation's INDEX; the
//! direction is `from_me` (the creator compared with our own ids), never the record's `isIncoming` (unreliable in both
//! directions); the call's own time is `record.start_time` in SECONDS (the mutation's `timestamp` is when it was
//! written); the record's participants may name OUR OWN ids, so "the other person" skips both.
//!
//! WHERE THE LINE GOES, in this order:
//! 1. the conversation the first line of this call was filed under (`call_lines`, [`crate::store::call_lines`]), but only
//!    when it FITS the record (F3, INTEROP-7: call ids are chosen by the calling side, and every member of a group sees a
//!    group call's id): a group record takes only the group it names; a 1:1 record only a direct chat of its creator or of
//!    one of the other persons it lists, in either form of that person (`started::forms`). A row that does not fit is
//!    ignored and the record is placed by its own rules below;
//! 2. a group call: the group the record names;
//! 3. an incoming 1:1 call: the creator, as his current LID when the store maps his number (the chats are keyed by
//!    LID);
//! 4. an OUTGOING 1:1 call (the creator is us): the one participant that is not us. None or several: no line, a count
//!    (H2: filing it under our own id would be wrong).
//!
//! The interceptor's zone does the same the other way round ([`rehome_lines`], F3 DUR-9): a ringing or missed line whose
//! call id the log already filed goes to the chat the log chose (when it fits), so log-then-interceptor makes ONE line.
//!
//! WHAT IT SAYS (D12's table; every line `Origin::OwnOtherDevice`, the only origin that may say answered, and the state
//! only moves up, so a late log never turns answered into missed, and a late ANSWERED moves a line the ring's deadline
//! made missed up to answered, whenever it comes: D2's "missed until the phone says"): incoming CONNECTED or ACCEPTEDELSEWHERE -> answered,
//! REJECTED -> declined, MISSED or CANCELLED -> missed; outgoing CONNECTED -> answered, REJECTED -> declined, MISSED,
//! CANCELLED or UNAVAILABLE -> unanswered; every other result says nothing (a count).
//!
//! HOW IT IS SHOWN (his answer: "Show them Quietly but it should be in list of chats ... if there is no chat - it should
//! be added"). Three forms, by the chat's CUT (`chat_cut`: the newest clear or delete of the chat this backend knows, from
//! Kourier's own confirmed `ClearConversation` and the phone's `clearChat` / `deleteChat`) and by whether Kourier already
//! holds a line of the call:
//! - a call at or below the cut, and a call in a chat with no known cut, is a HISTORY row: quiet by the contract (never
//!   announced), sorts at the call's time, and what a clear took away stays gone (the core drops a history row at or
//!   below its cut, and one of a chat the user deleted: history is not news);
//! - a call AFTER the cut is a LIVE row (`silent`, `read`: never announced, never counted): a live row brings a deleted
//!   chat back into the list, so a call the phone's log reports is in its chat whether or not Kourier was online to see
//!   it ring (INTEROP-4);
//! - a call whose line the interceptor (or an earlier record) already filed (`call_lines` has the id and it fits), and
//!   which started no more than [`SETTLES_MS`] ago (a line can ring no longer: the core caps a ring at two minutes), is
//!   a LIVE row too, but `read: false` and not silent: it settles that line as the interceptor's own terminate or
//!   deadline would, so a Missed whose hang-up was lost becomes unread and is announced, and Answered or Declined count
//!   nothing (INTEROP-8). An older filed call is not news (the core's line has settled long ago, and after a `Rebuild`
//!   or a lost row a replay must not announce every old missed call): it takes the forms above. Below the cut it stays
//!   a history row.
//!
//! A CALL FROM BEFORE THE LINK (F13, INTEROP-3, his answer "Only calls after link"): the phone's older call log reaches
//! this device at the link (the full sync of `regular` and the push's `callLogRecords`) and names every person called in
//! the last months. A record whose time (its start, else the mutation's write time) is strictly before the pairing
//! (`account_link`, written by `appstate::startup::link_in`) adds NO chat: it lands only in a chat the backend already
//! holds (`features_stated`, in either form of the person; the line goes to THAT form), else it is `Unplaced`, counted,
//! and nothing is written (a kept record of the push is marked taken up either way). A call after the link keeps the rule
//! below. A store that saw no link treats nothing as old.
//!
//! The chat is ADDED when the core has none: the zone states the chat's features first (`features::state`, a
//! `ConversationUpsert` that creates it, with `cannot_send: NotAccepted` because a call line is no start) and what the
//! phone said about it before (H5). The line is `read` because the log does not say which calls the phone had shown (the
//! contract: "not known" is `true`); a missed call Kourier saw ring is unread through the interceptor's own line. NO form
//! goes through the unarchive of a counting message ([`crate::appstate::zone::with_unarchive`]): a replay of old calls
//! must not bring archived chats back (H6, tested at the handler and at the take-up).
//!
//! Nothing here formats an upstream value: no JID, number or id reaches a log or an error.

use kourier_backend_api::{BackendEvent, Envelope, IncomingMessage};
use kourier_db::rusqlite::{Connection, Transaction};
use kourier_model::{CallDirection, CallLine, CallMedia, CallState, ConvKey, Origin, UserId};
use wacore::store::error::StoreError;
use wacore::types::events::CallLogSync;
use wacore_binary::Jid;
use waproto::whatsapp as wa;
use waproto::whatsapp::call_log_record::CallResult;

use crate::features;
use crate::map::{self, OwnIds};
use crate::send::{known, started};
use crate::store::{call_lines, chat_state, resyncs};

/// One call-log record, as plain Kourier data (the handler builds it from upstream's event on its own thread; the zone
/// decides on it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seen {
    /// The call's id, from the mutation's index.
    pub(crate) call_id: String,
    /// Who started the call (from the index); `None` when the id is no person.
    pub(crate) creator: Option<UserId>,
    /// This account placed the call (the creator compared with our ids by upstream).
    pub(crate) from_me: bool,
    /// The group the record names, when it names a group.
    pub(crate) group: Option<ConvKey>,
    /// The record's participants that are persons, in record order (our own ids may be among them).
    pub(crate) others: Vec<UserId>,
    /// `Some(true)` video, `Some(false)` voice, `None` the record does not say.
    pub(crate) video: Option<bool>,
    /// The record's result, if it has one.
    pub(crate) result: Option<CallResult>,
    /// The call's start (`record.start_time`, SECONDS).
    pub(crate) started_s: Option<i64>,
    /// When the mutation was written (ms): the call's time only when the record has no start.
    pub(crate) written_ms: i64,
}

impl Seen {
    /// The record of upstream's `CallLogSync`.
    pub(crate) fn from_sync(sync: &CallLogSync) -> Self {
        Self::of_record(
            &sync.record,
            sync.call_id.clone(),
            map::person_of(&sync.call_creator_jid),
            sync.from_me,
            sync.timestamp.timestamp_millis(),
        )
    }

    /// A record of the history push (`history_deferred` kind 1, W6a1 A6b): there is no mutation, so the
    /// record must name its call, its creator and its own start by itself; `None` when it does not (a record with
    /// no id, no creator that is a person, or no start would be filed under a guess or at a made-up time). The
    /// direction is the creator compared with our ids, as upstream does for the app-state record: never `isIncoming`.
    pub(crate) fn from_record(record: &wa::CallLogRecord, own: &OwnIds) -> Option<Self> {
        let call_id = record.call_id.clone().filter(|id| !id.is_empty())?;
        let creator = record
            .call_creator_jid
            .as_deref()?
            .parse::<Jid>()
            .ok()
            .and_then(|jid| map::person_of(&jid))?;
        record.start_time.filter(|seconds| *seconds > 0)?;
        let from_me = own.is_own(&creator);
        Some(Self::of_record(record, call_id, Some(creator), from_me, 0))
    }

    /// What both sources share: the group, the participants, the video flag, the result and the start.
    fn of_record(
        record: &wa::CallLogRecord,
        call_id: String,
        creator: Option<UserId>,
        from_me: bool,
        written_ms: i64,
    ) -> Self {
        Self {
            call_id,
            creator,
            from_me,
            group: record
                .group_jid
                .as_deref()
                .and_then(|jid| jid.parse::<Jid>().ok())
                .and_then(|jid| map::conv_of(&jid))
                .filter(|conv| matches!(conv, ConvKey::Group(_))),
            others: record
                .participants
                .iter()
                .filter_map(|p| p.user_jid.as_deref()?.parse::<Jid>().ok())
                .filter_map(|jid| map::person_of(&jid))
                .collect(),
            video: record.is_video,
            result: record.call_result,
            started_s: record.start_time,
            written_ms,
        }
    }
}

/// What a record came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// A line was delivered.
    Line,
    /// The result says nothing a line shows (D12's table).
    Silent,
    /// No chat could be named for the call (an outgoing call with no or several other persons, a creator that is no
    /// person), or the call is from before the link and no chat of the person exists (F13).
    Unplaced,
    /// The contract refused the line (a hostile call id).
    Refused,
}

/// How many records of each verdict a group commit saw (logged as numbers).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Counts {
    pub(crate) lines: usize,
    pub(crate) silent: usize,
    pub(crate) unplaced: usize,
    pub(crate) refused: usize,
}

impl Counts {
    pub(crate) fn add(&mut self, verdict: Verdict) {
        match verdict {
            Verdict::Line => self.lines += 1,
            Verdict::Silent => self.silent += 1,
            Verdict::Unplaced => self.unplaced += 1,
            Verdict::Refused => self.refused += 1,
        }
    }

    /// Whether there is anything to log.
    pub(crate) fn any(&self) -> bool {
        self.lines + self.silent + self.unplaced + self.refused > 0
    }
}

/// What [`place`] made of one record.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Placed {
    /// The core events, in order: the chat's statement (and its settings) when it needs one, then the line.
    pub(crate) events: Vec<BackendEvent>,
    pub(crate) verdict: Verdict,
}

impl Placed {
    fn none(verdict: Verdict) -> Self {
        Self {
            events: Vec::new(),
            verdict,
        }
    }
}

/// D12's table: the state a record's result gives a call, by who placed it.
fn state_of(from_me: bool, result: Option<CallResult>) -> Option<CallState> {
    Some(match (from_me, result?) {
        (false, CallResult::CONNECTED | CallResult::ACCEPTEDELSEWHERE) => CallState::Answered,
        (false, CallResult::REJECTED) => CallState::Declined,
        (false, CallResult::MISSED | CallResult::CANCELLED) => CallState::Missed,
        (true, CallResult::CONNECTED) => CallState::Answered,
        (true, CallResult::REJECTED) => CallState::Declined,
        (true, CallResult::MISSED | CallResult::CANCELLED | CallResult::UNAVAILABLE) => {
            CallState::Unanswered
        }
        _ => return None,
    })
}

/// The call's time (ms): its own start, else when the mutation was written; never later than `now` (a start in the
/// future would pin the line at the end of the chat for good).
fn time_of(seen: &Seen, now: i64) -> i64 {
    let at = seen
        .started_s
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| seconds.checked_mul(1000))
        .unwrap_or(seen.written_ms);
    if at <= 0 { now } else { at.min(now) }
}

/// A number as its current LID when the store maps it (the chats are keyed by LID), else as given.
fn homed(tx: &Transaction<'_>, user: &UserId) -> Result<UserId, StoreError> {
    if user.scheme() == "pn"
        && let Some(lid) = known::current_lid(tx, user.value())?
        && let Ok(lid) = UserId::from_parts("lid", &lid)
    {
        return Ok(lid);
    }
    Ok(user.clone())
}

/// How long after a call's start a line of it can still be RINGING in the core (ms): the core caps any ring at two
/// minutes. A log signal for a filed call settles it live and unread only inside this window (INTEROP-8); later the line
/// has settled by its own deadline, and a replay (a snapshot, a `Rebuild`'s take-up) must not announce an old missed call.
const SETTLES_MS: i64 = 2 * 60_000;

/// Where a record's line goes.
struct Home {
    conv: ConvKey,
    /// The chat is the one `call_lines` holds for this call: a line of it exists (the interceptor's, or an earlier
    /// record's), and this record settles it.
    filed: bool,
}

/// Whether `filed`, the conversation `call_lines` holds for a call, is a direct chat of one of `persons`, in either form
/// of that person (INTEROP-7, DUR-9).
fn is_chat_of(conn: &Connection, filed: &ConvKey, persons: &[&UserId]) -> Result<bool, StoreError> {
    if !matches!(filed, ConvKey::Direct(_)) {
        return Ok(false);
    }
    let forms = started::forms(conn, filed)?;
    Ok(persons
        .iter()
        .any(|person| forms.contains(&ConvKey::Direct((*person).clone()))))
}

/// The chat the line of 1:1 call `call_id` was filed under (`call_lines`), when it is a direct chat of `person` in
/// either form of him; `None` for no line, a group's line and another person's chat (a call id is the caller's
/// choice, INTEROP-7). The interceptor asks it for an accept of one of OUR devices (`calls::own_answer`): our device's
/// word settles the line Kourier shows and makes no new one.
pub(crate) fn filed_chat_of(
    conn: &Connection,
    call_id: &str,
    person: &UserId,
) -> Result<Option<ConvKey>, StoreError> {
    let Some(key) = call_lines::conv_of(conn, call_id)? else {
        return Ok(None);
    };
    let Ok(filed) = ConvKey::parse(&key) else {
        return Ok(None);
    };
    Ok(is_chat_of(conn, &filed, &[person])?.then_some(filed))
}

/// The conversation `call_lines` holds for the record's call, when it FITS the record (the module doc's first rule).
fn filed_conversation(
    tx: &Transaction<'_>,
    seen: &Seen,
    own: &OwnIds,
) -> Result<Option<ConvKey>, StoreError> {
    let Some(key) = call_lines::conv_of(tx, &seen.call_id)? else {
        return Ok(None);
    };
    let Ok(filed) = ConvKey::parse(&key) else {
        return Ok(None);
    };
    let fits = match &seen.group {
        Some(group) => &filed == group,
        None => {
            let persons: Vec<&UserId> = seen
                .creator
                .iter()
                .chain(seen.others.iter())
                .filter(|person| !own.is_own(person))
                .collect();
            is_chat_of(tx, &filed, &persons)?
        }
    };
    Ok(fits.then_some(filed))
}

/// The conversation of the call (the module doc's order), or `None` when none can be named.
fn conversation(
    tx: &Transaction<'_>,
    seen: &Seen,
    own: &OwnIds,
) -> Result<Option<Home>, StoreError> {
    if let Some(conv) = filed_conversation(tx, seen, own)? {
        return Ok(Some(Home { conv, filed: true }));
    }
    let home = |conv: ConvKey| Some(Home { conv, filed: false });
    if let Some(group) = &seen.group {
        return Ok(home(group.clone()));
    }
    if seen.from_me {
        let mut peers: Vec<UserId> = Vec::new();
        for other in seen.others.iter().filter(|other| !own.is_own(other)) {
            let other = homed(tx, other)?;
            if !peers.contains(&other) {
                peers.push(other);
            }
        }
        return Ok(match peers.as_slice() {
            [one] => home(ConvKey::Direct(one.clone())),
            _ => None,
        });
    }
    Ok(match &seen.creator {
        Some(creator) if !own.is_own(creator) => home(ConvKey::Direct(homed(tx, creator)?)),
        _ => None,
    })
}

/// The newest clear or delete of `conv` this backend knows, in either form of the person (a clear is noted under the
/// form the chat was in). `None`: the backend knows of no clear. A cut far ahead of the clock needs no cap here: a call's
/// time is never later than `now` ([`time_of`]).
fn cut_of(tx: &Transaction<'_>, conv: &ConvKey) -> Result<Option<i64>, StoreError> {
    let mut cut: Option<i64> = None;
    for form in started::forms(tx, conv)? {
        cut = cut.max(chat_state::cut_of(tx, &form)?);
    }
    Ok(cut)
}

/// Whether a call at `at` (ms) is older than the link: this store's pairing time (`account_link`, written by the link's
/// own zone) is known and `at` is strictly before it. A store that saw no link treats nothing as old. The two clocks are
/// the phone's (the call) and ours (the link): a call within their skew of the link may fall on either side.
fn before_the_link(tx: &Transaction<'_>, at: i64) -> Result<bool, StoreError> {
    Ok(resyncs::linked_at(tx)?.is_some_and(|linked| at < linked))
}

/// The form of `conv` under which the backend holds a chat (a statement of any version, `started::was_stated`), in either
/// form of the person (`started::forms`; `conv` itself first), or `None` when it holds none.
fn existing_form(tx: &Transaction<'_>, conv: &ConvKey) -> Result<Option<ConvKey>, StoreError> {
    for form in started::forms(tx, conv)? {
        if started::was_stated(tx, &form)? {
            return Ok(Some(form));
        }
    }
    Ok(None)
}

/// The zone of one record (inside the caller's transaction; the event writer's group commit): decides its
/// conversation, notes it in `call_lines` and gives the core events. See the module doc.
pub(crate) fn place(
    tx: &Transaction<'_>,
    seen: &Seen,
    own: &OwnIds,
    now: i64,
) -> Result<Placed, StoreError> {
    let Some(state) = state_of(seen.from_me, seen.result) else {
        return Ok(Placed::none(Verdict::Silent));
    };
    let Some(Home { conv, filed }) = conversation(tx, seen, own)? else {
        return Ok(Placed::none(Verdict::Unplaced));
    };
    let at = time_of(seen, now);
    // A call from before the link adds no chat (F13, INTEROP-3, his answer "Only calls after link"): it lands only in a chat
    // the backend already holds, in whichever form of the person that chat has; else it is counted and nothing is written.
    let conv = if before_the_link(tx, at)? {
        let Some(existing) = existing_form(tx, &conv)? else {
            return Ok(Placed::none(Verdict::Unplaced));
        };
        existing
    } else {
        conv
    };
    let group = matches!(conv, ConvKey::Group(_));
    let direction = if seen.from_me {
        CallDirection::Outgoing
    } else {
        CallDirection::Incoming
    };
    let mut signal = CallLine::signal(direction, group, state, at);
    if !group && let Some(video) = seen.video {
        let media = if video {
            CallMedia::Video
        } else {
            CallMedia::Voice
        };
        signal = signal.with_media(media, at);
    }
    if let Some(caller) = seen
        .creator
        .as_ref()
        .filter(|creator| !seen.from_me && !own.is_own(creator))
    {
        signal = signal.with_by(homed(tx, caller)?, at);
    }
    let mut message = IncomingMessage::call(
        conv.clone(),
        &seen.call_id,
        Origin::OwnOtherDevice { device: None },
        signal,
        at,
        now,
    );
    // The form (the module doc): by the chat's cut and by whether a line of this call is already filed.
    let cut = cut_of(tx, &conv)?;
    let below = cut.is_some_and(|cut| at <= cut);
    let settling = filed && now.saturating_sub(at) <= SETTLES_MS;
    let line = if below || !(settling || cut.is_some()) {
        // History: quiet, and what a clear took away stays gone. The log does not say which calls the phone had shown:
        // "not known" is read (the contract of a history row).
        message.silent = true;
        message.read = true;
        BackendEvent::HistoryBatch {
            conv: conv.clone(),
            messages: vec![message],
            has_more: false,
            progress: None,
        }
    } else if settling {
        // Live, settling the line that is still ringing there as the interceptor's own terminate would (INTEROP-8):
        // unread when it ends missed, announced like that; answered and declined count nothing.
        message.read = false;
        BackendEvent::Message(message)
    } else {
        // Live, after the cut (INTEROP-4): quiet, and it brings a deleted chat back into the list.
        message.silent = true;
        message.read = true;
        BackendEvent::Message(message)
    };
    // The contract's own check, whole: ids, the call line's rules and its `local`. A line the core would refuse for
    // good is never written, and a hostile call id is never noted.
    let (envelope, _waiter) = Envelope::durable(vec![line.clone()]);
    if envelope.validate().is_err() {
        return Ok(Placed::none(Verdict::Refused));
    }
    call_lines::note(tx, &seen.call_id, &conv.encode(), at, now)?;
    let mut events = features::state(tx, &conv)?;
    events.push(line);
    Ok(Placed {
        events,
        verdict: Verdict::Line,
    })
}

/// The interceptor's zone, before [`note_lines`] (F3, DUR-9): a 1:1 call line whose call id the log already filed goes to
/// the chat the log chose when that is a direct chat of the same person in the other form (the log homes a number to his
/// current LID, the stanza may say the number), so log-then-interceptor makes ONE line. A group line has one home; a row
/// that is not a person's chat (or not the same person's) is left where the interceptor put it. `None` when nothing moved.
pub(crate) fn rehome_lines(
    tx: &Transaction<'_>,
    events: &[BackendEvent],
) -> Result<Option<Vec<BackendEvent>>, StoreError> {
    let mut out: Option<Vec<BackendEvent>> = None;
    for (i, event) in events.iter().enumerate() {
        let BackendEvent::Message(message) = event else {
            continue;
        };
        let (Some(line), ConvKey::Direct(person)) = (message.body.call_line(), &message.conv)
        else {
            continue;
        };
        if line.group {
            continue;
        }
        let Some(id) = message.local.strip_prefix(CallLine::DIRECT_PREFIX) else {
            continue;
        };
        let Some(key) = call_lines::conv_of(tx, id)? else {
            continue;
        };
        let Ok(filed) = ConvKey::parse(&key) else {
            continue;
        };
        if filed == message.conv || !is_chat_of(tx, &filed, &[person])? {
            continue;
        }
        let out = out.get_or_insert_with(|| events.to_vec());
        if let BackendEvent::Message(moved) = &mut out[i] {
            moved.conv = filed;
        }
    }
    Ok(out)
}

/// Notes the call lines among `events` in `call_lines` (the interceptor's zone: the event writer calls it for every row
/// that names a chat), so the log's outcome of such a call lands in the same chat. The first note of a call id stands.
pub(crate) fn note_lines(
    tx: &Transaction<'_>,
    events: &[BackendEvent],
    now: i64,
) -> Result<(), StoreError> {
    for event in events {
        let BackendEvent::Message(message) = event else {
            continue;
        };
        let Some(line) = message.body.call_line() else {
            continue;
        };
        let prefix = if line.group {
            CallLine::GROUP_PREFIX
        } else {
            CallLine::DIRECT_PREFIX
        };
        if let Some(id) = message.local.strip_prefix(prefix) {
            call_lines::note(tx, id, &message.conv.encode(), message.sent_at, now)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
