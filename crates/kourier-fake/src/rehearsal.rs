//! The rehearsal network (T1a): the fake that PLAYS WhatsApp for `cargo xtask wa-run rehearse`,
//! the run tool's end-to-end rehearsal. It is a fake for a debug build with a throwaway profile
//! and no network at all; nothing here is a model of what WhatsApp really sends.
//!
//! What it plays, in the order a real run meets it:
//!
//! - The descriptor is WhatsApp's WITHOUT the risk notice (the warning page is K-A's and has UI
//!   tests of its own; no dev command ticks it, and none should).
//! - The QR login shows two codes of [`Timings::code`] each; the phone scans the second.
//! - At the pairing the script honours `BackendConfig::expect_number_suffix` like the real
//!   backend does since W2c: a tail the account's number does not end in fails the login at the
//!   `qr` step with `NotTheExpectedAccount`, and the account is never made
//!   ([`Script::honour_number_tail`]).
//! - A fresh link pushes its history, as WhatsApp does: batches that say `has_more` over about
//!   twenty seconds with a pause of [`Timings::pause`] between them, then the ends. The core
//!   shows that as the history phase of its transfer banner, which is what the tool waits on.
//!   The chats come with their chunks, one pause apart (the direct chat first, the group's with
//!   its first batch), so the number of conversations grows while the push runs.
//! - A resumed account goes online and replays nothing: the script cursor is durable in the
//!   account's `backend.db`, and the script is the same for a fresh start and a resumed one
//!   (the default script of the network), so a restart goes on where the account stopped.
//! - [`FakeControl::arrive`] puts one text from the other member into the first chat of a kind
//!   of the RUNNING account, on cue (the tool's `receive` and `group-receive` steps).
//! - [`FakeControl::phone`] plays the rest of what a phone does on cue (T2, T-5): the settings it
//!   changes (archive, pin, mute), a call that rings and is answered on the phone, and the other
//!   person's react, edit and delete of the text that arrived last. Nothing of it is a frame the
//!   run tool sends: the budget counts only what the fake TAKES from the app.
//!
//! The ids have the shapes the real backend gives: `pn:<12 digits>`, `lid:<15 digits>`,
//! `wag:<18 digits>`. Every value is synthetic. The two tails are 8 digits, the right one is the
//! end of [`OWN_NUMBER`], and neither is a substring of any other value here (a test pins it), so
//! a search of the files of a rehearsal for a tail finds a leak and nothing else.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_model::{ConvKey, MemberRole, UserId};
use tokio::sync::mpsc;

use crate::network::FakeControl;
use crate::script::{LoginScript, Script, ScriptError, Step};
use crate::scripts::{T0, contact, conversation, member, remote, whatsapp_features};

/// The other person's name in the rehearsal (the run tool searches the files of a rehearsal for it).
pub const PEER_NAME: &str = "Rehearsal Peerperson";
/// The rehearsal group's title.
pub const GROUP_NAME: &str = "Rehearsal Grouptitle";
/// The rehearsal account's own profile name.
pub const OWN_NAME: &str = "Rehearsal Testphone";
/// The rehearsal account's number (digits only).
pub const OWN_NUMBER: &str = "999000735193";
/// The rehearsal account's LID.
pub const OWN_LID: &str = "270118840216503";
/// The other person's number.
pub const PEER_NUMBER: &str = "999000481726";
/// The other person's LID.
pub const PEER_LID: &str = "270118840277122";
/// The rehearsal group's number (the part of its id after `wag:`).
pub const GROUP_NUMBER: &str = "120363999000026113";
/// The stems of the texts of the pushed history (`<stem> <n>`): the run tool searches the files of
/// a rehearsal for them, so a text that leaks shows.
pub const OLDER_WORDS: &str = "older words";
/// See [`OLDER_WORDS`].
pub const NEWER_WORDS: &str = "newer words";
/// See [`OLDER_WORDS`].
pub const NEWEST_WORDS: &str = "newest words";
/// See [`OLDER_WORDS`].
pub const GROUP_WORDS: &str = "group words";
/// The stem of the text of an arrival (`<stem> <n>`).
pub const ARRIVAL_WORDS: &str = "Rehearsal arrival";
/// How the ids of the pushed history's messages begin.
pub const HISTORY_ID_PREFIX: &str = "3EB0H";
/// How the id of an arrival begins.
pub const ARRIVAL_ID_PREFIX: &str = "3EB0AR";
/// How the id of a call the rehearsal's phone rings begins (`<prefix><n>`; T2 F9, READS-9: a bare number could not be
/// told from any other text by the run tool's leak search).
pub const CALL_ID_PREFIX: &str = "REHCALL";
/// The name the rehearsal account gives its device.
pub const DEVICE_NAME: &str = "Kourier (rehearsal)";
/// The last digits of [`OWN_NUMBER`] a rehearsal types for the right phone.
pub const RIGHT_TAIL: &str = "00735193";
/// Last digits no number of the rehearsal ends in: the wrong phone.
pub const WRONG_TAIL: &str = "50926184";

/// How long the rehearsal's walks take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timings {
    /// Each QR code is shown this long (two codes; the phone scans the second).
    pub code: Duration,
    /// The pause between two batches of the pushed history (four pauses: about twenty seconds).
    pub pause: Duration,
}

impl Timings {
    /// What a rehearsal run uses: codes of three seconds, pauses of five.
    pub const REAL: Self = Self {
        code: Duration::from_secs(3),
        pause: Duration::from_secs(5),
    };
}

/// Which kind of chat an arrival goes into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
    /// The first 1:1 chat of the account.
    Direct,
    /// The first group of the account.
    Group,
}

impl std::fmt::Display for Arrival {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Direct => "direct",
            Self::Group => "group",
        })
    }
}

/// Why [`FakeControl::arrive`] put nothing anywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ArriveError {
    /// No account of this network is running.
    #[error("no account is running")]
    NoRunningAccount,
    /// The running account has no chat of that kind yet (its history has not come, or the
    /// script has none).
    #[error("the account has no {0} chat yet")]
    NoChat(Arrival),
    /// The account is stopping.
    #[error("the account is stopping")]
    Stopping,
    /// The phone plays no such act in a chat of that kind (a call is a 1:1 chat's).
    #[error("the phone plays no {0} in a {1} chat")]
    Unsupported(PhoneAct, Arrival),
    /// The act is on the text that arrived last in the chat, and none has this start.
    #[error("{0} needs a text that arrived in that chat in this start")]
    NothingArrived(PhoneAct),
}

/// What the rehearsal's phone does on cue besides writing a text ([`FakeControl::phone`]). The
/// settings are the phone's own acts (the app is told as the network tells it); the three
/// `Peer` acts are the other person's, on the text that arrived last in the chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhoneAct {
    /// The chat is archived on the phone.
    Archive,
    /// The chat is taken out of the archive on the phone.
    Unarchive,
    /// The chat is pinned (first of the pinned chats).
    Pin,
    /// The chat is unpinned.
    Unpin,
    /// The chat is muted for ever.
    Mute,
    /// The mute is taken off.
    Unmute,
    /// The other person calls into the chat (1:1): the call rings, then it is answered on the
    /// phone ([`CALL_RINGS_FOR`]).
    Call,
    /// The other person reacts to the text that arrived last.
    PeerReact,
    /// The other person edits the text that arrived last.
    PeerEdit,
    /// The other person deletes the text that arrived last, for everyone.
    PeerDelete,
}

impl PhoneAct {
    /// Every act.
    pub const ALL: [Self; 10] = [
        Self::Archive,
        Self::Unarchive,
        Self::Pin,
        Self::Unpin,
        Self::Mute,
        Self::Unmute,
        Self::Call,
        Self::PeerReact,
        Self::PeerEdit,
        Self::PeerDelete,
    ];

    /// The act's name (what the dev control spells).
    pub fn name(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::Unarchive => "unarchive",
            Self::Pin => "pin",
            Self::Unpin => "unpin",
            Self::Mute => "mute",
            Self::Unmute => "unmute",
            Self::Call => "call",
            Self::PeerReact => "peer_react",
            Self::PeerEdit => "peer_edit",
            Self::PeerDelete => "peer_delete",
        }
    }

    /// Whether the act is on the text that arrived last.
    fn needs_a_text(self) -> bool {
        matches!(self, Self::PeerReact | Self::PeerEdit | Self::PeerDelete)
    }
}

impl std::fmt::Display for PhoneAct {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// How long a call of the phone rings before it is answered on the phone (long enough for a
/// driver that looks every second to see the ring).
pub const CALL_RINGS_FOR: Duration = Duration::from_millis(2500);

/// The variable the run tool sets, for ONE start and with exactly the value `1`, when it starts the
/// app with `--call-builtin` (the real backend: `kourier-whatsapp` `calls::BUILTIN_ENV`; the tool's
/// test reads that file and this one and pins both copies).
pub(crate) const CALL_BUILTIN_ENV: &str = "KOURIER_WA_CALL_BUILTIN";
/// The WARN line the real backend prints when the switch is on (`session/client.rs`). The fake plays
/// no call handler, but it prints the same words, so the run tool's proof that the order REACHED the
/// app's process, and only that start's, is walked end to end in the rehearsal too.
pub(crate) const CALL_BUILTIN_WARNING: &str = "KOURIER_WA_CALL_BUILTIN is set: upstream's own call handler will answer calls (a debug test switch)";

/// Whether the switch is on: exactly `1`, as the real backend reads it. Any other value, a longer
/// one included, is off.
pub(crate) fn call_builtin_on(value: Option<&str>) -> bool {
    value == Some("1")
}

/// What the fake does at the start of a run-tool rehearsal's account about the switch: it prints
/// the real backend's warning when, and only when, the environment holds `1`.
pub(crate) fn say_call_builtin() {
    if call_builtin_on(std::env::var(CALL_BUILTIN_ENV).ok().as_deref()) {
        tracing::warn!("{CALL_BUILTIN_WARNING}");
    }
}

/// What the fake does when its phone rings a call into the account: with the switch on, it prints the real backend's
/// INFO line that an OFFER was handed to upstream's handler (`kourier-whatsapp` `calls.rs`, T2 F7), so the run tool's
/// report counts the offers of a `--call-builtin` start in the rehearsal too. Fixed words, no id.
pub(crate) fn say_call_offer_to_upstream() {
    if call_builtin_on(std::env::var(CALL_BUILTIN_ENV).ok().as_deref()) {
        tracing::info!("upstream's call handler is given a call offer (a debug test switch)");
    }
}

/// The one text whose ack the fake holds back the FIRST time it takes it (T2 F8): the run tool's resend text
/// (`xtask/src/wa_run.rs` `RESEND_TEXT`; a test there pins both copies). The tool kills the app right after that frame,
/// so the kill comes BEFORE the ack, and the next start's resend of the same message is real: the same id reaches the
/// fake again and is dropped as a repeat (WhatsApp very likely does), and it is acked then.
pub const HELD_ACK_TEXT: &str =
    "Kourier test run: a text that is sent again after a kill (step resend)";

/// Whether the ack of this request is the one the fake holds back (when the server takes a new copy of it).
pub(crate) fn holds_the_ack_of(kind: &kourier_backend_api::OutgoingKind) -> bool {
    matches!(
        kind,
        kourier_backend_api::OutgoingKind::Message {
            body: kourier_model::Body::Text(text),
            ..
        } if text.body == HELD_ACK_TEXT
    )
}

/// What the fake says when the server meets an id it already holds (the repeat of a frame whose ack never reached the
/// core): fixed words, no id. The run tool counts the line, so the rehearsal proves the resend kept its id.
pub(crate) fn say_repeat_dropped() {
    tracing::info!("a send frame repeated a message the server already holds");
}

/// The kind word of a request's frame in the two count lines (the real backend's `send::attempt::frame_kind`: the
/// run tool's test pins both copies of the words).
pub(crate) fn frame_word(kind: &kourier_backend_api::OutgoingKind) -> &'static str {
    use kourier_backend_api::OutgoingKind;
    match kind {
        OutgoingKind::Message { .. } => "message",
        OutgoingKind::React { .. } => "reaction",
        OutgoingKind::Edit { .. } => "edit",
        OutgoingKind::Delete {
            for_everyone: true, ..
        } => "revoke",
        OutgoingKind::Delete { .. } => "other",
    }
}

/// The first of the two COUNT LINES the run tool counts (T2 F1): a frame the fake took from the app, as the real
/// backend says one right after its write (`kourier-whatsapp` `send/attempt.rs`, `write_once`). Fixed words, a kind and
/// nothing else; the rehearsal's fake prints them for every send and act it takes and for a timer set, so the walk
/// compares the tool's wire count with the fake's own (`FakeControl::frames`).
pub(crate) fn say_frame_written(kind: &str) {
    tracing::info!(kind = %kind, "a send frame was written");
}

/// The second count line: how that frame was answered (`acked`, `refused` with the 463, `nacked` with its code, 0 for
/// none) and whether the core sends the local again (`again`: `no`, or the seconds it waits at least; T2 G1), said when
/// the fake's answer leaves for the core.
pub(crate) fn say_frame_answered(outcome: &str, code: u16, again: &str) {
    tracing::info!(outcome = %outcome, code, again = %again, "a send frame was answered");
}

/// The `again` of the second count line for the error kind a fake's answer carries: the core's own rule, as the real
/// backend reads it off the error it answers with (`kourier-whatsapp` `send/attempt.rs` `again_of`): `no` unless the kind
/// is retryable, else the seconds of its `retry_after`, rounded up (0 for none).
pub(crate) fn again_words(kind: &kourier_model::ErrorKind) -> String {
    if kind.is_retryable() {
        let wait = kind.retry_after().unwrap_or_default();
        (wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).to_string()
    } else {
        "no".to_owned()
    }
}

/// The outcome, the code and the `again` of the second count line for a fake's answer to a transmit.
pub(crate) fn answered_words(
    outcome: &kourier_backend_api::TransmitOutcome,
) -> (&'static str, u16, String) {
    use kourier_backend_api::TransmitOutcome;
    use kourier_model::ErrorKind;
    match outcome {
        TransmitOutcome::Sent { .. } | TransmitOutcome::SentAs { .. } => {
            ("acked", 0, "no".to_owned())
        }
        TransmitOutcome::Partial { .. } => ("nacked", 0, "no".to_owned()),
        TransmitOutcome::Failed(error) => {
            let again = again_words(&error.kind);
            match error.kind {
                ErrorKind::ReachOutRefused => ("refused", 463, again),
                ErrorKind::RateLimited { .. } => ("nacked", 429, again),
                _ => ("nacked", 0, again),
            }
        }
    }
}

/// A chat the account has, and the member who writes into it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KnownChat {
    pub(crate) conv: ConvKey,
    pub(crate) author: UserId,
    /// The text that arrived last in this chat in this start (`FakeControl::arrive`): its
    /// network id and its number. What the other person's react, edit and delete are on.
    pub(crate) newest: Option<Arrived>,
}

/// A text [`FakeControl::arrive`] put into a chat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Arrived {
    pub(crate) local: String,
    pub(crate) n: i64,
}

/// What [`FakeControl::arrive`] and [`FakeControl::phone`] hand the actor: write one text from
/// `author` into `conv` (`act: None`), or play `act` there.
#[derive(Debug)]
pub(crate) struct Injection {
    pub(crate) conv: ConvKey,
    pub(crate) author: UserId,
    pub(crate) act: Option<PhoneAct>,
}

/// A running account's door: where an arrival is sent, and the chats it has so far.
#[derive(Debug)]
pub(crate) struct Door {
    pub(crate) uuid: kourier_model::AccountUuid,
    pub(crate) tx: mpsc::UnboundedSender<Injection>,
    pub(crate) chats: Arc<Mutex<Vec<KnownChat>>>,
}

impl FakeControl {
    /// Puts one text from the other member into the first chat of `kind` of the RUNNING account
    /// (the first account that runs, when several do), as a live message of the network: a new
    /// unread message, not history. The chat must exist already: an account whose history has not
    /// arrived has none.
    ///
    /// # Errors
    /// [`ArriveError::NoRunningAccount`] when no account of this network runs,
    /// [`ArriveError::NoChat`] when it has no chat of that kind yet, [`ArriveError::Stopping`]
    /// when it is shutting down.
    pub fn arrive(&self, kind: Arrival) -> Result<(), ArriveError> {
        self.inject(kind, None)
    }

    /// The rehearsal's phone plays `act` in the first chat of `kind` of the RUNNING account (T2,
    /// T-5): a setting it changes, a call, or the other person's react, edit or delete of the text
    /// that arrived last there. Nothing is played by itself and nothing here is a frame of the
    /// run tool's budget ([`FakeControl::frames`] counts what the fake takes FROM the app).
    ///
    /// # Errors
    /// The errors of [`FakeControl::arrive`], [`ArriveError::Unsupported`] for a call into a
    /// group, and [`ArriveError::NothingArrived`] for an act on a text when none arrived in
    /// that chat in this start.
    pub fn phone(&self, kind: Arrival, act: PhoneAct) -> Result<(), ArriveError> {
        if act == PhoneAct::Call && kind == Arrival::Group {
            return Err(ArriveError::Unsupported(act, kind));
        }
        self.inject(kind, Some(act))
    }

    fn inject(&self, kind: Arrival, act: Option<PhoneAct>) -> Result<(), ArriveError> {
        let doors = self.doors.lock().unwrap_or_else(PoisonError::into_inner);
        let door = doors.first().ok_or(ArriveError::NoRunningAccount)?;
        let chat = {
            let mut chats = door.chats.lock().unwrap_or_else(PoisonError::into_inner);
            let chat = chats
                .iter_mut()
                .find(|c| match kind {
                    Arrival::Direct => matches!(c.conv, ConvKey::Direct(_)),
                    Arrival::Group => matches!(c.conv, ConvKey::Group(_)),
                })
                .ok_or(ArriveError::NoChat(kind))?;
            match act {
                // A text is on its way: a cue right behind it (an edit of it) finds it. The
                // actor puts the real id there when it commits the text; the cues are taken
                // in the order they were sent.
                None => {
                    chat.newest = Some(Arrived {
                        local: String::new(),
                        n: 0,
                    });
                }
                Some(act) if act.needs_a_text() && chat.newest.is_none() => {
                    return Err(ArriveError::NothingArrived(act));
                }
                Some(_) => {}
            }
            chat.clone()
        };
        door.tx
            .send(Injection {
                conv: chat.conv,
                author: chat.author,
                act,
            })
            .map_err(|_| ArriveError::Stopping)
    }

    /// An account started: its door is open until the returned guard drops.
    pub(crate) fn open_door(
        self: &Arc<Self>,
        uuid: kourier_model::AccountUuid,
        tx: mpsc::UnboundedSender<Injection>,
        chats: Arc<Mutex<Vec<KnownChat>>>,
    ) -> DoorGuard {
        self.doors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Door { uuid, tx, chats });
        DoorGuard {
            control: self.clone(),
            uuid,
        }
    }
}

/// Closes the door of an account when its actor ends.
pub(crate) struct DoorGuard {
    control: Arc<FakeControl>,
    uuid: kourier_model::AccountUuid,
}

impl Drop for DoorGuard {
    fn drop(&mut self) {
        self.control
            .doors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|d| d.uuid != self.uuid);
    }
}

/// The chats a list of delivered events creates, in order, each with the member who writes into
/// it: a direct chat's other user, a group's first member who is not `own`.
pub(crate) fn chats_of<'a>(
    events: impl IntoIterator<Item = &'a BackendEvent>,
    own: &[UserId],
) -> Vec<KnownChat> {
    let mut out = Vec::new();
    for event in events {
        let BackendEvent::ConversationUpsert(patch) = event else {
            continue;
        };
        let author = match &patch.key {
            ConvKey::Direct(user) => Some(user.clone()),
            ConvKey::Group(_) => patch.members.as_ref().and_then(|list| {
                list.upsert
                    .iter()
                    .map(|m| &m.user)
                    .find(|u| !own.contains(u))
                    .cloned()
            }),
            ConvKey::Broadcast(_) | ConvKey::Channel(_) => None,
        };
        if let Some(author) = author
            && !out.iter().any(|c: &KnownChat| c.conv == patch.key)
        {
            out.push(KnownChat {
                conv: patch.key.clone(),
                author,
                newest: None,
            });
        }
    }
    out
}

fn id(scheme: &str, value: &str) -> Result<UserId, ScriptError> {
    Ok(UserId::from_parts(scheme, value)?)
}

/// The rehearsal account's script (see the module docs). The same script plays a fresh start
/// and a resumed one.
///
/// # Errors
/// Never in practice: the ids are constants of a valid shape.
pub fn script(timings: Timings) -> Result<Script, ScriptError> {
    let own_pn = id("pn", OWN_NUMBER)?;
    let own_lid = id("lid", OWN_LID)?;
    let peer = id("pn", PEER_NUMBER)?;
    let direct = ConvKey::Direct(peer.clone());
    let group = ConvKey::Group(format!("wag:{GROUP_NUMBER}").into());
    // What the WhatsApp backend states for chats the phone started (D16: one source with the backend).
    let features = whatsapp_features(false);
    let group_features = whatsapp_features(true);

    // History rows are old, read and silent: the push of a link is not news.
    let history = |conv: &ConvKey, from: i64, n: i64, text: &str| {
        (0..n)
            .map(|i| {
                let mut m = remote(
                    conv,
                    &peer,
                    &format!("{HISTORY_ID_PREFIX}{from:04}{i:03}"),
                    T0 + (from + i) * 60_000,
                    &format!("{text} {i}"),
                );
                m.silent = true;
                m.read = true;
                m
            })
            .collect::<Vec<_>>()
    };
    let batch = |conv: &ConvKey, messages, has_more, progress| {
        Step::Deliver(crate::script::Delivery::new(vec![
            BackendEvent::HistoryBatch {
                conv: conv.clone(),
                messages,
                has_more,
                progress: Some(progress),
            },
        ]))
    };
    let pause = Step::Sleep(timings.pause);

    let mut s = Script::new("rehearsal", vec![own_pn, own_lid]);
    // It plays WhatsApp as the real run sees it: no roaming setting is written yet (W6).
    s.roaming_writes = None;
    // The group's chat comes with its first chunk, a pause after the first chat: chats of a
    // real push appear as its chunks arrive, so the number of conversations GROWS while the
    // phase says history (the run tool's `history-interrupt` waits for that growth).
    let group_chunk = Step::Deliver(crate::script::Delivery::new(vec![
        BackendEvent::ConversationUpsert(conversation(
            &group,
            Some(GROUP_NAME),
            Some(vec![
                member(&peer, MemberRole::Admin),
                member(&s.own_ids[0], MemberRole::Member),
            ]),
            &group_features,
        )),
        BackendEvent::HistoryBatch {
            conv: group.clone(),
            messages: history(&group, 100, 3, GROUP_WORDS),
            has_more: true,
            progress: Some(0.5),
        },
    ]));
    s.number = Some(OWN_NUMBER.to_owned());
    s.display_name = Some(OWN_NAME.to_owned());
    s.device_name = Some(DEVICE_NAME.to_owned());
    s.login = LoginScript::Qr {
        ask_device_name: false,
        codes: 2,
        valid_for: timings.code,
        scanned_at: Some(1),
        linked_as: None,
    };
    s.honour_number_tail = true;
    s.steps = vec![
        Step::Deliver(crate::script::Delivery::new(vec![
            BackendEvent::ConversationUpsert(conversation(&direct, None, None, &features)),
            contact(&peer, Some(PEER_NAME), None),
        ])),
        batch(&direct, history(&direct, 0, 3, OLDER_WORDS), true, 0.25),
        pause.clone(),
        group_chunk,
        pause.clone(),
        batch(&direct, history(&direct, 200, 2, NEWER_WORDS), true, 0.75),
        pause.clone(),
        batch(&group, history(&group, 300, 2, NEWEST_WORDS), true, 0.9),
        pause,
        batch(&direct, Vec::new(), false, 1.0),
        batch(&group, Vec::new(), false, 1.0),
    ];
    Ok(s)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_call_builtin_switch_is_on_for_exactly_the_value_one() {
        use super::call_builtin_on;
        assert!(call_builtin_on(Some("1")));
        for off in [
            None,
            Some(""),
            Some("0"),
            Some("11"),
            Some("true"),
            Some(" 1"),
            Some("1 "),
        ] {
            assert!(!call_builtin_on(off), "{off:?}");
        }
    }

    use super::*;

    const SENTINEL: &str = "000000000000000";

    #[test]
    fn the_rehearsal_ids_are_jid_shaped_and_its_tails_collide_with_nothing() {
        // The shapes of the real backend's ids: a number of 12 digits, a LID of 15, a group of 18.
        let shapes = [
            (OWN_NUMBER, 12),
            (PEER_NUMBER, 12),
            (OWN_LID, 15),
            (PEER_LID, 15),
            (GROUP_NUMBER, 18),
        ];
        for (value, len) in shapes {
            assert_eq!(value.len(), len, "{value}");
            assert!(value.bytes().all(|b| b.is_ascii_digit()), "{value}");
        }
        for (scheme, value) in [
            ("pn", OWN_NUMBER),
            ("lid", OWN_LID),
            ("pn", PEER_NUMBER),
            ("lid", PEER_LID),
        ] {
            let user = id(scheme, value).expect("id");
            assert_eq!((user.scheme(), user.value()), (scheme, value));
        }

        // The tails: at least 8 digits, the right one ends the account's number, no other number
        // of the rehearsal ends in either, and neither is a substring of any other value (nor of
        // the other tail, nor the sentinel), so a search for a tail finds a leak and nothing else.
        for tail in [RIGHT_TAIL, WRONG_TAIL] {
            assert!(tail.len() >= 8, "{tail}");
            assert!(tail.bytes().all(|b| b.is_ascii_digit()), "{tail}");
        }
        assert!(OWN_NUMBER.ends_with(RIGHT_TAIL));
        assert!(!OWN_NUMBER.ends_with(WRONG_TAIL));
        let others = [OWN_LID, PEER_NUMBER, PEER_LID, GROUP_NUMBER];
        for tail in [RIGHT_TAIL, WRONG_TAIL] {
            for other in others {
                assert!(!other.contains(tail), "{tail} is inside {other}");
            }
            assert!(!tail.contains(SENTINEL));
        }
        assert!(!WRONG_TAIL.contains(RIGHT_TAIL) && !RIGHT_TAIL.contains(WRONG_TAIL));
        assert!(!OWN_NUMBER.contains(WRONG_TAIL));
        for value in [OWN_NUMBER, OWN_LID, PEER_NUMBER, PEER_LID, GROUP_NUMBER] {
            assert!(!value.contains(SENTINEL), "no value holds the sentinel");
        }
    }

    #[test]
    fn the_real_timings_walk_a_login_of_six_seconds_and_a_history_of_twenty() {
        let script = script(Timings::REAL).expect("script");
        let pauses: Duration = script
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::Sleep(d) => Some(*d),
                _ => None,
            })
            .sum();
        assert_eq!(pauses, Duration::from_secs(20));
        match script.login {
            LoginScript::Qr {
                codes,
                valid_for,
                scanned_at,
                ..
            } => assert_eq!(
                (codes, valid_for, scanned_at),
                (2, Duration::from_secs(3), Some(1))
            ),
            other => panic!("{other:?}"),
        }
        assert!(script.honour_number_tail);
    }

    /// T2 G1: the fake's `again` is the core's own rule on the error it answers with, the real backend's rule: `no` for
    /// an ack and for every kind the core does not retry, else the seconds of the kind's `retry_after`, rounded up (0 for
    /// none). Mutant: the retryable test negated.
    #[test]
    fn the_fakes_again_is_no_unless_the_core_sends_the_frame_again() {
        use kourier_backend_api::TransmitOutcome;
        use kourier_model::{BackendError, ErrorKind};
        let secs = |s: u64| Some(std::time::Duration::from_secs(s));
        let words = |kind: ErrorKind| {
            let (word, code, again) =
                answered_words(&TransmitOutcome::Failed(BackendError::new(kind, "x")));
            (word, code, again)
        };
        assert_eq!(
            words(ErrorKind::RateLimited {
                retry_after: secs(300)
            }),
            ("nacked", 429, "300".to_owned())
        );
        assert_eq!(
            words(ErrorKind::Transient { retry_after: None }),
            ("nacked", 0, "0".to_owned())
        );
        assert_eq!(
            words(ErrorKind::Transient {
                retry_after: Some(std::time::Duration::from_millis(1500))
            }),
            ("nacked", 0, "2".to_owned())
        );
        assert_eq!(
            words(ErrorKind::ReachOutRefused),
            ("refused", 463, "no".to_owned())
        );
        assert_eq!(
            words(ErrorKind::Unsupported),
            ("nacked", 0, "no".to_owned())
        );
        assert_eq!(
            words(ErrorKind::PeerUnregistered),
            ("nacked", 0, "no".to_owned())
        );
        assert_eq!(
            answered_words(&TransmitOutcome::Sent {
                server_at: None,
                per_recipient: Vec::new()
            }),
            ("acked", 0, "no".to_owned())
        );
    }

    #[test]
    fn the_known_chats_are_the_first_of_each_kind_with_the_member_who_writes() {
        let own = vec![id("pn", OWN_NUMBER).expect("id")];
        let script = script(Timings::REAL).expect("script");
        let events: Vec<&BackendEvent> = script
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::Deliver(d) => Some(&d.events),
                _ => None,
            })
            .flatten()
            .collect();
        let chats = chats_of(events, &own);
        assert_eq!(chats.len(), 2);
        assert_eq!(
            chats[0].conv,
            ConvKey::Direct(id("pn", PEER_NUMBER).expect("id"))
        );
        assert_eq!(chats[0].author, id("pn", PEER_NUMBER).expect("id"));
        assert!(matches!(chats[1].conv, ConvKey::Group(_)));
        assert_eq!(chats[1].author, id("pn", PEER_NUMBER).expect("id"));
    }
}
