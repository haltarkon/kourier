//! The handler's messages: a group's notice and the placeholder of a message that cannot be read (W3b,
//! chunks C6b and C6c; a group's notice grew in W6a2, chunk B5). Pure: an upstream event in, the contract's events and
//! the acts that need the group's row out.
//!
//! These are not rows of the inbox. Upstream dispatches the event from the stanza's own task BEFORE it
//! acknowledges the stanza (a group notification's ack is deferred until the handler returns,
//! `src/handlers/notification/groups.rs`; an undecryptable message is dispatched before its retry
//! receipt and ack, `src/message/retry.rs`), so the handler writes the events with the group commit
//! and waits (rule (a), `events::EventWriter`): nothing is acknowledged before the notice is in
//! `core_outbox`. A redelivery gives the same events: a notice has a deterministic id, a placeholder
//! is under the message's own id, and the core ignores a second message with the same
//! `(conversation, id)`.
//!
//! WHAT A NOTICE IS (W6a2, D9 and D10): the timeline's line, and the change of the group that the notice carries. The
//! group's whole state is the fetch's (`groups`, the batch at each start), but a notice may carry a piece of it, so a
//! busy group is not fetched at every notice:
//! - what needs NO row rides in [`Noticed::events`]: the pairs it vouches for, the timer (with the notification's time as
//!   its version), the description's text, and the line itself;
//! - what needs the group's row is an [`Act`], made into events IN the zone that writes the notice (`groups::zone`): the
//!   title (an older rename never sets an older title, M5), a member delta (only when the core holds the whole list), a
//!   change of the rules or of our place in the group (the features are stated again from the row, M4), and the mark
//!   "fetch this group" for what a notice cannot carry (`modify`, `create`, the community family, an incomplete
//!   participant list, a change while the rules are unknown).
//!
//! A promote or a demote is the ACTION's role, never the participant's `type` attribute (scout T1): upstream reads `type`
//! and says `Participant` when the server wrote none.

use std::time::Duration;

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_model::ids::validate_local;
use kourier_model::{
    Body, ConvKey, ConversationPatch, Member, MemberRole, Origin as Sender, Patch, Placeholder,
    SystemEvent, UserId,
};
use wacore::stanza::groups::{GroupNotificationAction as Action, GroupParticipantInfo};
use wacore::types::events::{DecryptFailMode, GroupUpdate, UndecryptableMessage};
use wacore_binary::Jid;

use crate::groups::{Change, DESCRIPTION_CAP};
use crate::map::{self, OwnIds};
use crate::names;

/// A group's name is cut to this many characters in a notice or a history chat (WhatsApp's own limit is far
/// below).
pub(crate) const TITLE_CAP: usize = 512;

/// What a group's notification does to the group's row, decided in the zone that writes the notice (`groups::zone`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Act {
    /// A rename: the name (cut, never blank) and the time in seconds it is dated (the notice's `s_t`, else the
    /// notification's `t`, scout T6). The core is told it only when it is not older than the title it was told (M5).
    Title { title: String, at_s: u64 },
    /// A member delta: people added or whose role changed, people removed. Stated only when the core holds the group's
    /// whole list; otherwise the group is marked for a fetch (a delta on no list would show a partial one).
    Members {
        upsert: Vec<Member>,
        remove: Vec<UserId>,
    },
    /// One bit of the group's rules or of our place: the row follows it when it holds rules, else the group is marked
    /// for a fetch (a rule stated without the others would claim them off).
    Rule(Change),
    /// The group was ended for everyone: terminated, and we are no member of it.
    Deleted,
    /// We were removed from the group (or left it): `NotMember`.
    Gone,
    /// We were put into the group (again): a member, with nothing known of the group until it is fetched.
    Back,
    /// The notice cannot carry the change: the group is marked for a fetch (the fetch policy, chunk B6, asks under its
    /// bucket and never before `Online`).
    Fetch,
}

/// A group's notification as the handler writes it: the events that need no row and the acts that do.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Noticed {
    /// The group.
    pub(crate) conv: ConvKey,
    /// The pairs it vouches for, the upserts it carries (timer, description) and the line, in that order; each passes
    /// the contract's id check. The zone's own upserts go in front of the line.
    pub(crate) events: Vec<BackendEvent>,
    /// What the group's row decides.
    pub(crate) acts: Vec<Act>,
}

/// One action of a `w:gp2` notification, made into events and acts. `None` for an action this build leaves out
/// (invites, membership requests and the rest: K8c) and for a chat that is no group.
///
/// A notification without `participant` (a change the server made itself) gives no line: `by: None`
/// would read as "You" (the contract's own-account rule), and a made-up author would be a lie. Its timer, title and
/// the state it changes still reach the group.
pub(crate) fn group_notice(update: &GroupUpdate, now_ms: u64, own: &OwnIds) -> Option<Noticed> {
    let conv = map::conv_of(&update.group_jid).filter(|conv| matches!(conv, ConvKey::Group(_)))?;
    let actor = update.participant.as_ref().and_then(map::person_of);
    let ours = actor.as_ref().is_some_and(|actor| own.is_own(actor));
    // `None` is "You": only ever the account's own act.
    let by = actor.clone().filter(|_| !ours);

    let mut events: Vec<BackendEvent> = pairs(update, own)
        .into_iter()
        .map(|(from, into)| BackendEvent::IdentityMerged { from, into })
        .collect();
    let mut acts: Vec<Act> = Vec::new();
    // The notification's time in seconds: the title's date when the notice has none, the timer's version.
    let t = u64::try_from(update.timestamp.timestamp()).unwrap_or(0);

    let event = match &*update.action {
        Action::Add { participants, .. } => {
            let users = people(participants);
            if users.is_empty() {
                None
            } else {
                acts.push(Act::Members {
                    upsert: users
                        .iter()
                        .map(|user| Member {
                            user: user.clone(),
                            role: MemberRole::Member,
                        })
                        .collect(),
                    remove: Vec::new(),
                });
                if names_us(participants, own) {
                    acts.push(Act::Back);
                }
                if ours && matches!(users.as_slice(), [only] if actor.as_ref() == Some(only)) {
                    // We joined (a link, an accepted invitation): the account's own join, as Signal's.
                    Some(SystemEvent::Joined)
                } else {
                    Some(SystemEvent::MembersAdded { by, users })
                }
            }
        }
        Action::Remove { participants, .. } => {
            let users = people(participants);
            if !users.is_empty() {
                acts.push(Act::Members {
                    upsert: Vec::new(),
                    remove: users.clone(),
                });
            }
            if names_us(participants, own) {
                acts.push(Act::Gone);
            }
            match users.as_slice() {
                [] => None,
                [only] if actor.as_ref() == Some(only) => {
                    Some(SystemEvent::MemberLeft { user: only.clone() })
                }
                _ => Some(SystemEvent::MembersRemoved { by, users }),
            }
        }
        // The role is the ACTION (scout T1), whatever `type` says; a super admin is never stated (D10).
        Action::Promote { participants } => {
            roles(participants, MemberRole::Admin, own, &mut acts);
            None
        }
        Action::Demote { participants } => {
            roles(participants, MemberRole::Member, own, &mut acts);
            None
        }
        Action::Subject {
            subject,
            subject_time,
            ..
        } => {
            let title: String = subject.chars().take(TITLE_CAP).collect();
            // The group's name is the chat's title (fix F11, the lead's Q8.3): the chat is renamed as well as told,
            // whoever changed it, a server-made change included; an empty name changes nothing (WhatsApp has no
            // unnamed group). The zone sets it unless it is older than the one the core was told (M5).
            if !title.trim().is_empty() {
                acts.push(Act::Title {
                    title: title.clone(),
                    at_s: subject_time.unwrap_or(t),
                });
            }
            Some(SystemEvent::TitleChanged { by, title })
        }
        Action::Description { description, .. } => {
            let mut patch = ConversationPatch::new(conv.clone());
            patch.description = description
                .as_deref()
                .map(|text| text.chars().take(DESCRIPTION_CAP).collect::<String>())
                .filter(|text| !text.trim().is_empty())
                .map_or(Patch::Clear, Patch::Set);
            events.push(BackendEvent::ConversationUpsert(patch));
            Some(SystemEvent::DescriptionChanged { by })
        }
        Action::Create { .. } => {
            acts.push(Act::Fetch);
            Some(SystemEvent::GroupCreated { by })
        }
        Action::Ephemeral { expiration, .. } => {
            let timer = (*expiration > 0).then(|| Duration::from_secs(u64::from(*expiration)));
            let mut patch = ConversationPatch::new(conv.clone());
            patch.timer = timer.map_or(Patch::Clear, Patch::Set);
            // The notification's time orders the changes (the highest version wins in the core).
            patch.timer_version = u32::try_from(t).ok().filter(|version| *version > 0);
            events.push(BackendEvent::ConversationUpsert(patch));
            Some(SystemEvent::TimerChanged { by, timer })
        }
        Action::Announce => rule(&mut acts, Change::Announcements(true)),
        Action::NotAnnounce => rule(&mut acts, Change::Announcements(false)),
        Action::Locked { .. } => rule(&mut acts, Change::Locked(true)),
        Action::Unlocked => rule(&mut acts, Change::Locked(false)),
        Action::Suspended => rule(&mut acts, Change::Suspended(true)),
        Action::Unsuspended => rule(&mut acts, Change::Suspended(false)),
        Action::MemberAddMode { mode } => match mode.as_str() {
            "admin_add" => rule(&mut acts, Change::MembersAdminOnly(true)),
            "all_member_add" => rule(&mut acts, Change::MembersAdminOnly(false)),
            // A mode this build does not know: the fetch says what it means.
            _ => fetch(&mut acts),
        },
        Action::Delete { .. } => {
            acts.push(Act::Deleted);
            None
        }
        // What a notice cannot carry: a member's new number, and the community family (the parent, an elevated
        // admin of a subgroup).
        Action::Modify { .. }
        | Action::Link { .. }
        | Action::Unlink { .. }
        | Action::LinkedGroupPromote { .. }
        | Action::LinkedGroupDemote { .. } => fetch(&mut acts),
        _ => None,
    };
    // The server says the participant list of this stanza was incomplete: the fetch has the whole list.
    if update.has_incomplete_participant_information && !acts.contains(&Act::Fetch) {
        acts.push(Act::Fetch);
    }

    // The notice itself: needs the one who did it.
    if let (Some(event), Some(actor)) = (event, actor.clone()) {
        let at = update.timestamp.timestamp_millis();
        let origin = if ours {
            Sender::OwnOtherDevice { device: None }
        } else {
            Sender::Remote {
                author: actor,
                device: None,
            }
        };
        let mut notice = IncomingMessage::new(
            conv.clone(),
            notice_id(update).as_str(),
            origin,
            Body::System(event),
            at,
            i64::try_from(now_ms).unwrap_or(i64::MAX),
        );
        // The core measures its windows from the server's time, which is the notification's.
        notice.server_at = Some(at);
        events.push(BackendEvent::Message(notice));
    }
    let mut events = valid(events).unwrap_or_default();
    // The actor's own name rides with a notice that made something (D8): his, never ours, and never a name for the server's
    // own change (no `participant`). The zone states it only when the core was not told it ([`names::keep_changed`]).
    if let Some(actor) = actor.filter(|_| !ours)
        && let Some(name) = update.notify.as_deref().and_then(names::clean)
        && (!events.is_empty() || !acts.is_empty())
    {
        let at = names::place(&events);
        events.insert(at, names::push_name_event(actor, name));
    }
    (!events.is_empty() || !acts.is_empty()).then_some(Noticed { conv, events, acts })
}

/// A rule changed: the act, and no line (the contract has no notice kind for it).
fn rule(acts: &mut Vec<Act>, change: Change) -> Option<SystemEvent> {
    acts.push(Act::Rule(change));
    None
}

/// A change the notice cannot carry: the group is fetched, and no line.
fn fetch(acts: &mut Vec<Act>) -> Option<SystemEvent> {
    acts.push(Act::Fetch);
    None
}

/// A promote or a demote: the people get `role`; when one of them is us, our place changes as well.
fn roles(list: &[GroupParticipantInfo], role: MemberRole, own: &OwnIds, acts: &mut Vec<Act>) {
    let users = people(list);
    if !users.is_empty() {
        acts.push(Act::Members {
            upsert: users
                .into_iter()
                .map(|user| Member { user, role })
                .collect(),
            remove: Vec::new(),
        });
    }
    if names_us(list, own) {
        acts.push(Act::Rule(Change::OwnAdmin(role == MemberRole::Admin)));
    }
}

/// Whether one of `list` is us, by his id, his number or his LID (the list may name us by either form).
fn names_us(list: &[GroupParticipantInfo], own: &OwnIds) -> bool {
    list.iter().any(|info| {
        [
            Some(&info.jid),
            info.phone_number.as_ref(),
            info.lid.as_ref(),
        ]
        .into_iter()
        .flatten()
        .filter_map(map::person_of)
        .any(|person| own.is_own(&person))
    })
}

/// The id of a notice: the notification's own id and the action's place in it, so the redelivery of the
/// notification is the same row. A notification without an id (or with one the contract refuses) is named
/// by its time instead.
fn notice_id(update: &GroupUpdate) -> String {
    let index = update.action_index;
    update
        .notification_id
        .as_ref()
        .map(|id| format!("gp:{id}:{index}"))
        .filter(|local| validate_local(local).is_ok())
        .unwrap_or_else(|| format!("gp:{}:{index}", update.timestamp.timestamp()))
}

/// The people a participant list names, each once, in the stanza's order.
fn people(list: &[GroupParticipantInfo]) -> Vec<UserId> {
    let mut users: Vec<UserId> = Vec::new();
    for info in list {
        if let Some(user) = map::person_of(&info.jid)
            && !users.contains(&user)
        {
            users.push(user);
        }
    }
    users
}

/// The pairs of one number and one LID that the notification vouches for, as `(number, lid)`: the actor
/// with the number the server gave for him (`participant_pn`), and a member with his other id. Never one
/// of our own ids; each pair once.
fn pairs(update: &GroupUpdate, own: &OwnIds) -> Vec<(UserId, UserId)> {
    let mut found: Vec<(UserId, UserId)> = Vec::new();
    let mut vouch = |a: Option<&Jid>, b: Option<&Jid>| {
        if let (Some(a), Some(b)) = (a, b)
            && let Some(pair) = map::vouched_pair(a, b, own)
            && !found.contains(&pair)
        {
            found.push(pair);
        }
    };
    vouch(update.participant.as_ref(), update.participant_pn.as_ref());
    if let Action::Add { participants, .. } | Action::Remove { participants, .. } = &*update.action
    {
        for info in participants {
            vouch(Some(&info.jid), info.phone_number.as_ref());
            vouch(Some(&info.jid), info.lid.as_ref());
        }
    }
    found
}

/// The placeholder of a message that could not be decrypted, under the message's own id: the real message
/// with that id replaces it when it arrives (a resend the sender answers to the retry receipt upstream
/// sends right after this event, `src/message/retry.rs`).
///
/// - `decrypt_fail_mode == Hide`: WhatsApp marks the failures of a reaction, an edit and the other
///   protocol messages so: nothing is shown.
/// - `is_unavailable` (view-once, which a companion never receives, and a few kinds of other chats): a
///   placeholder that does not promise a resend (`resendable: false`).
/// - A message of our own phone, a chat that is no conversation and a sender that is no person: nothing.
pub(crate) fn undecryptable(lost: &UndecryptableMessage, now_ms: u64) -> Option<Vec<BackendEvent>> {
    if lost.decrypt_fail_mode == DecryptFailMode::Hide {
        return None;
    }
    let source = &lost.info.source;
    if source.is_from_me {
        return None;
    }
    let conv = map::conv_of(&source.chat)?;
    let author = map::person_of(&source.sender)?;
    let device = (source.sender.device > 0).then_some(u32::from(source.sender.device));
    let mut message = IncomingMessage::new(
        conv,
        lost.info.id.as_str(),
        Sender::Remote { author, device },
        Body::Placeholder(Placeholder::Undecryptable {
            resendable: !lost.is_unavailable,
        }),
        lost.info.timestamp.timestamp_millis(),
        i64::try_from(now_ms).unwrap_or(i64::MAX),
    );
    message.server_at = Some(lost.info.timestamp.timestamp_millis());
    valid(vec![BackendEvent::Message(message)])
}

/// The events, once every id passes the contract's own check.
fn valid(events: Vec<BackendEvent>) -> Option<Vec<BackendEvent>> {
    if events.is_empty() || events.iter().any(|event| event.validate_ids().is_err()) {
        return None;
    }
    Some(events)
}

#[cfg(test)]
mod tests;
