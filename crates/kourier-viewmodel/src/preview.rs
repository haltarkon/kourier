//! One-line summaries of message content: conversation-list previews, quote snippets and the
//! texts of system rows.

use std::time::Duration;

use kourier_model::{
    AttachmentKind, Body, CallDirection, CallLine, CallMedia, CallState, Deletion, DisappearKind,
    Expiry, Message, Placeholder, QuoteSnapshot, SystemEvent, Text, UserId,
};

use crate::i18n::{trc, trfc, trnc};
use crate::names::{Names, single_line};
use crate::richtext;

/// Characters a preview keeps; the list elides further in QML.
pub const PREVIEW_MAX_CHARS: usize = 160;

/// A one-line summary with an optional icon (Breeze icon name) for media and special rows.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct Summary {
    /// The text, one line, at most [`PREVIEW_MAX_CHARS`] characters.
    pub text: String,
    /// Icon name, empty for plain text.
    pub icon: String,
}

crate::safe_debug!(Summary { icon });

impl Summary {
    fn new(text: impl Into<String>, icon: &str) -> Self {
        Self {
            text: clip(&single_line(&text.into())),
            icon: icon.to_owned(),
        }
    }
}

fn clip(s: &str) -> String {
    match s.char_indices().nth(PREVIEW_MAX_CHARS) {
        Some((i, _)) => format!("{}…", s[..i].trim_end()),
        None => s.to_owned(),
    }
}

/// `text` as one line of at most [`PREVIEW_MAX_CHARS`] characters (banners, quotes).
pub fn clip_line(text: &str) -> String {
    clip(&single_line(text))
}

/// Plain one-line form of a text: mentions as `@Name`, spoilers hidden as `■■■`.
pub fn text_line(text: &Text, names: &dyn Names) -> String {
    let rich = richtext::render(text, names);
    if rich.spoilers.is_empty() {
        return single_line(&rich.plain);
    }
    let units: Vec<u16> = rich.plain.encode_utf16().collect();
    let mut out = String::new();
    let mut at = 0usize;
    for s in &rich.spoilers {
        let (start, end) = (s.start as usize, (s.start + s.len) as usize);
        out.push_str(&String::from_utf16_lossy(&units[at..start]));
        out.push_str("■■■");
        at = end;
    }
    out.push_str(&String::from_utf16_lossy(&units[at..]));
    single_line(&out)
}

/// Icon and label of an attachment kind.
pub fn attachment_label(kind: AttachmentKind, count: usize) -> Summary {
    let n = count as u64;
    let (text, icon) = match kind {
        AttachmentKind::Image if count > 1 => (
            trnc(
                "@info:preview, %1 how many photos",
                "%1 photo",
                "%1 photos",
                n,
            ),
            "photo",
        ),
        AttachmentKind::Image => (
            trc("@info:preview attachment type", "Photo").into_owned(),
            "photo",
        ),
        AttachmentKind::Video => (
            trc("@info:preview attachment type", "Video").into_owned(),
            "video-x-generic",
        ),
        AttachmentKind::Gif => (
            trc("@info:preview attachment type", "GIF").into_owned(),
            "video-x-generic",
        ),
        AttachmentKind::Voice => (
            trc("@info:preview attachment type", "Voice message").into_owned(),
            "media-record",
        ),
        AttachmentKind::Audio => (
            trc("@info:preview attachment type", "Audio").into_owned(),
            "audio-x-generic",
        ),
        AttachmentKind::File => (
            trc("@info:preview attachment type", "File").into_owned(),
            "mail-attachment",
        ),
    };
    Summary::new(text, icon)
}

/// Summary of a message body (not deleted).
pub fn body_summary(body: &Body, names: &dyn Names) -> Summary {
    match body {
        Body::Text(t) => Summary::new(text_line(t, names), ""),
        Body::Media {
            attachments,
            caption,
        } => {
            let first = attachments.first();
            let kind = first.map_or(AttachmentKind::File, |a| a.kind);
            let same = attachments.iter().all(|a| a.kind == kind);
            let mut label = attachment_label(
                if same { kind } else { AttachmentKind::File },
                attachments.len(),
            );
            let caption = caption
                .as_ref()
                .or_else(|| first.and_then(|a| a.caption.as_ref()))
                .filter(|c| !c.body.trim().is_empty());
            if let Some(c) = caption {
                label.text = clip(&text_line(c, names));
            } else if kind == AttachmentKind::File
                && let Some(name) = first.and_then(|a| a.file_name.as_deref())
            {
                label.text = clip(&single_line(name));
            }
            label
        }
        Body::Sticker(_) => Summary::new(trc("@info:preview kind of message", "Sticker"), "smiley"),
        Body::Contact(c) => Summary::new(
            trfc(
                "@info:preview, %1 the name of a contact",
                "Contact: %1",
                &[c.name
                    .as_deref()
                    .unwrap_or(&trc("@item:intext a person without a name", "Unknown"))],
            ),
            "addressbook-details",
        ),
        Body::Location(l) => Summary::new(
            l.name
                .clone()
                .unwrap_or_else(|| trc("@info:preview kind of message", "Location").into_owned()),
            "mark-location",
        ),
        Body::Poll(p) => Summary::new(
            trfc(
                "@info:preview, %1 the question of a poll",
                "Poll: %1",
                &[&p.question],
            ),
            "view-list-text",
        ),
        Body::System(e) => Summary::new(system_text(e, names), ""),
        Body::Placeholder(p) => placeholder_summary(*p),
        Body::Unsupported { kind_hint, summary } => {
            unsupported_summary(kind_hint, summary.as_deref())
        }
    }
}

/// What stands in for content Kourier does not show, by the backend's `kind_hint`: what it
/// is and where to see it. The backend's own summary is added when it has one (an amount, a
/// question). Unknown hints get a general text.
pub fn unsupported_summary(kind_hint: &str, summary: Option<&str>) -> Summary {
    let (text, icon) = match kind_hint {
        "view-once" => (
            trc(
                "@info:preview content that is not shown",
                "View-once media. Open it on your phone.",
            ),
            "view-hidden",
        ),
        "payment" => (
            trc(
                "@info:preview content that is not shown",
                "Payment. See it on your phone.",
            ),
            "wallet-closed",
        ),
        "gift-badge" => (
            trc(
                "@info:preview content that is not shown",
                "Donation badge. See it on your phone.",
            ),
            "emblem-favorite",
        ),
        "poll" => (
            trc(
                "@info:preview content that is not shown",
                "Poll. Vote on your phone.",
            ),
            "view-statistics",
        ),
        "poll-ended" => (
            trc(
                "@info:preview content that is not shown",
                "A poll ended. See the result on your phone.",
            ),
            "view-statistics",
        ),
        "pin" => (
            trc(
                "@info:preview content that is not shown",
                "A message was pinned. See it on your phone.",
            ),
            "window-pin",
        ),
        "story-reply" => (
            trc(
                "@info:preview content that is not shown",
                "Reply to a story. See it on your phone.",
            ),
            "view-media-recent",
        ),
        "contact" => (
            trc(
                "@info:preview content that is not shown",
                "Contact card. Open it on your phone.",
            ),
            "addressbook-details",
        ),
        "sticker" => (
            trc(
                "@info:preview content that is not shown",
                "Sticker that could not be shown.",
            ),
            "smiley",
        ),
        "group-call" | "call" => (
            trc(
                "@info:preview content that is not shown",
                "Call. Calls are not available here.",
            ),
            "call-start",
        ),
        "newer-protocol" => (
            trc(
                "@info:preview content that is not shown",
                "This message needs a newer version of Kourier.",
            ),
            "update-none",
        ),
        _ => (
            trc(
                "@info:preview content that is not shown",
                "This kind of message cannot be shown here yet.",
            ),
            "help-about",
        ),
    };
    let detail = summary.map(single_line).filter(|d| !d.is_empty());
    match detail {
        Some(d) => Summary::new(format!("{text} {d}"), icon),
        None => Summary::new(text, icon),
    }
}

/// The short form of [`unsupported_summary`] for the conversation list ("View-once media").
pub fn unsupported_label(kind_hint: &str) -> Summary {
    let (text, icon) = match kind_hint {
        "view-once" => (
            trc("@info:preview kind of message", "View-once media"),
            "view-hidden",
        ),
        "payment" => (
            trc("@info:preview kind of message", "Payment"),
            "wallet-closed",
        ),
        "gift-badge" => (
            trc("@info:preview kind of message", "Donation badge"),
            "emblem-favorite",
        ),
        "poll" => (
            trc("@info:preview kind of message", "Poll"),
            "view-statistics",
        ),
        "poll-ended" => (
            trc("@info:preview kind of message", "Poll ended"),
            "view-statistics",
        ),
        "pin" => (
            trc("@info:preview kind of message", "Pinned message"),
            "window-pin",
        ),
        "story-reply" => (
            trc("@info:preview kind of message", "Reply to a story"),
            "view-media-recent",
        ),
        "contact" => (
            trc("@info:preview kind of message", "Contact card"),
            "addressbook-details",
        ),
        "sticker" => (trc("@info:preview kind of message", "Sticker"), "smiley"),
        "group-call" | "call" => (trc("@info:preview kind of message", "Call"), "call-start"),
        "newer-protocol" => (
            trc(
                "@info:preview kind of message",
                "Message for a newer version",
            ),
            "update-none",
        ),
        _ => (
            trc("@info:preview kind of message", "Unsupported message"),
            "help-about",
        ),
    };
    Summary::new(text, icon)
}

/// What the timer mark of a disappearing message says.
pub fn expiry_text(expiry: &Expiry) -> String {
    let timer = timer_label(expiry.timer);
    match expiry.kind {
        DisappearKind::AfterRead => trfc(
            "@info:status on a message, %1 a length of time",
            "Disappears %1 after it is read",
            &[&timer],
        ),
        DisappearKind::AfterSend => trfc(
            "@info:status on a message, %1 a length of time",
            "Disappears %1 after it was sent",
            &[&timer],
        ),
    }
}

/// The stand-in for a quote whose original is not on this device and that carried nothing to
/// show.
pub fn missing_quote() -> Summary {
    Summary::new(
        trc("@info:preview in a quote", "Original message not found"),
        "dialog-question",
    )
}

/// The stand-in for a quote whose original was deleted after it was quoted.
pub fn deleted_quote() -> Summary {
    Summary::new(
        trc("@info:preview in a quote", "Deleted message"),
        "trash-empty",
    )
}

/// Summary of a stand-in.
pub fn placeholder_summary(p: Placeholder) -> Summary {
    match p {
        Placeholder::Undecryptable { .. } => Summary::new(
            trc(
                "@info:preview stand-in for a message",
                "Message could not be decrypted",
            ),
            "dialog-error",
        ),
        Placeholder::ViewOnceOpened => Summary::new(
            trc("@info:preview kind of message", "View-once media"),
            "view-hidden",
        ),
        Placeholder::Expired => Summary::new(
            trc(
                "@info:preview stand-in for a message",
                "This message expired",
            ),
            "clock",
        ),
    }
}

/// Text of a tombstone.
pub fn deleted_text(deletion: &Deletion, own: bool) -> String {
    match deletion {
        Deletion::ForEveryone { .. } if own => trc(
            "@info:preview stand-in for a deleted message",
            "You deleted this message",
        )
        .into_owned(),
        _ => trc(
            "@info:preview stand-in for a deleted message",
            "This message was deleted",
        )
        .into_owned(),
    }
}

/// Summary of a whole message for previews and quotes.
pub fn message_summary(msg: &Message, names: &dyn Names) -> Summary {
    match &msg.deleted {
        Some(d) => Summary::new(deleted_text(d, msg.origin.is_own()), "trash-empty"),
        None => body_summary(&msg.body, names),
    }
}

/// Summary of a quote snapshot (when the quoted message is not loaded).
pub fn snapshot_summary(snap: &QuoteSnapshot, names: &dyn Names) -> Summary {
    if let Some(t) = snap.text.as_ref().filter(|t| !t.body.trim().is_empty()) {
        let icon = snap
            .attachment
            .map(|k| attachment_label(k, 1).icon)
            .unwrap_or_default();
        return Summary::new(text_line(t, names), &icon);
    }
    match (snap.attachment, &snap.file_name) {
        (Some(AttachmentKind::File), Some(name)) => Summary::new(name.clone(), "mail-attachment"),
        (Some(kind), _) => attachment_label(kind, 1),
        (None, _) => missing_quote(),
    }
}

fn list(users: &[UserId], names: &dyn Names) -> String {
    let names: Vec<String> = users.iter().map(|u| names.display_name(u)).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        // TRANSLATORS: %1 is one name or several joined with commas, %2 the last name.
        [a, b] => trfc(
            "@info a list of names, %1 and %2 names",
            "%1 and %2",
            &[a, b],
        ),
        [first @ .., last] => trfc(
            "@info a list of names, %1 and %2 names",
            "%1 and %2",
            &[&first.join(", "), last],
        ),
    }
}

/// Human form of a disappearing-message timer.
pub fn timer_label(timer: Duration) -> String {
    let s = timer.as_secs();
    match s {
        s if s % 604_800 == 0 => trnc("@info a length of time", "%1 week", "%1 weeks", s / 604_800),
        s if s % 86_400 == 0 => trnc("@info a length of time", "%1 day", "%1 days", s / 86_400),
        s if s % 3600 == 0 => trnc("@info a length of time", "%1 hour", "%1 hours", s / 3600),
        s if s % 60 == 0 => trnc("@info a length of time", "%1 minute", "%1 minutes", s / 60),
        s if s < 60 => trnc("@info a length of time", "%1 second", "%1 seconds", s),
        // A custom time that is no whole number of one unit: "1 hour 30 seconds".
        // TRANSLATORS: the parts of a length of time are joined with a space ("1 hour 30
        // seconds"): they are the "a length of time" messages above.
        s => {
            let parts = [
                (
                    s / 3600,
                    trnc("@info a length of time", "%1 hour", "%1 hours", s / 3600),
                ),
                (
                    s % 3600 / 60,
                    trnc(
                        "@info a length of time",
                        "%1 minute",
                        "%1 minutes",
                        s % 3600 / 60,
                    ),
                ),
                (
                    s % 60,
                    trnc("@info a length of time", "%1 second", "%1 seconds", s % 60),
                ),
            ];
            parts
                .into_iter()
                .filter(|(n, _)| *n > 0)
                .map(|(_, text)| text)
                .collect::<Vec<_>>()
                .join(" ")
        }
    }
}

/// The text of a system row. `by: None` means our own account, which has a message of its
/// own ("You added …"): in many languages the verb follows the person, so "You" cannot stand
/// where a name stands.
pub fn system_text(event: &SystemEvent, names: &dyn Names) -> String {
    match event {
        SystemEvent::MembersAdded { by: None, users } => trfc(
            "@info:status chat notice, the user did it, %1 who was added",
            "You added %1",
            &[&list(users, names)],
        ),
        SystemEvent::MembersAdded {
            by: Some(by),
            users,
        } => trfc(
            "@info:status chat notice, %1 who added, %2 who was added",
            "%1 added %2",
            &[&names.display_name(by), &list(users, names)],
        ),
        SystemEvent::MembersRemoved { by: None, users } => trfc(
            "@info:status chat notice, the user did it, %1 who was removed",
            "You removed %1",
            &[&list(users, names)],
        ),
        SystemEvent::MembersRemoved {
            by: Some(by),
            users,
        } => trfc(
            "@info:status chat notice, %1 who removed, %2 who was removed",
            "%1 removed %2",
            &[&names.display_name(by), &list(users, names)],
        ),
        SystemEvent::MemberLeft { user } => trfc(
            "@info:status chat notice, %1 a name",
            "%1 left the group",
            &[&names.display_name(user)],
        ),
        SystemEvent::TitleChanged { by: None, title } => trfc(
            "@info:status chat notice, the user did it, %1 the new name of the group",
            "You changed the group name to “%1”",
            &[&single_line(title)],
        ),
        SystemEvent::TitleChanged {
            by: Some(by),
            title,
        } => trfc(
            "@info:status chat notice, %1 who, %2 the new name of the group",
            "%1 changed the group name to “%2”",
            &[&names.display_name(by), &single_line(title)],
        ),
        SystemEvent::AvatarChanged { by: None } => trc(
            "@info:status chat notice, the user did it",
            "You changed the group photo",
        )
        .into_owned(),
        SystemEvent::AvatarChanged { by: Some(by) } => trfc(
            "@info:status chat notice, %1 who",
            "%1 changed the group photo",
            &[&names.display_name(by)],
        ),
        SystemEvent::DescriptionChanged { by: None } => trc(
            "@info:status chat notice, the user did it",
            "You changed the group description",
        )
        .into_owned(),
        SystemEvent::DescriptionChanged { by: Some(by) } => trfc(
            "@info:status chat notice, %1 who",
            "%1 changed the group description",
            &[&names.display_name(by)],
        ),
        SystemEvent::TimerChanged {
            by: None,
            timer: None,
        } => trc(
            "@info:status chat notice, the user did it",
            "You turned off disappearing messages",
        )
        .into_owned(),
        SystemEvent::TimerChanged {
            by: Some(by),
            timer: None,
        } => trfc(
            "@info:status chat notice, %1 who",
            "%1 turned off disappearing messages",
            &[&names.display_name(by)],
        ),
        SystemEvent::TimerChanged {
            by: None,
            timer: Some(t),
        } => trfc(
            "@info:status chat notice, the user did it, %1 a length of time",
            "You set the disappearing message timer to %1",
            &[&timer_label(*t)],
        ),
        SystemEvent::TimerChanged {
            by: Some(by),
            timer: Some(t),
        } => trfc(
            "@info:status chat notice, %1 who, %2 a length of time",
            "%1 set the disappearing message timer to %2",
            &[&names.display_name(by), &timer_label(*t)],
        ),
        SystemEvent::IdentityKeyChanged { user } => trfc(
            "@info:status chat notice, %1 a name",
            "Your safety number with %1 has changed",
            &[&names.display_name(user)],
        ),
        SystemEvent::SessionRefreshed { user } => trfc(
            "@info:status chat notice, %1 a name",
            "Secure session with %1 refreshed",
            &[&names.display_name(user)],
        ),
        SystemEvent::GroupCreated { by: None } => trc(
            "@info:status chat notice, the user did it",
            "You created the group",
        )
        .into_owned(),
        SystemEvent::GroupCreated { by: Some(by) } => trfc(
            "@info:status chat notice, %1 who",
            "%1 created the group",
            &[&names.display_name(by)],
        ),
        SystemEvent::MessageRequestAccepted => trc(
            "@info:status chat notice",
            "You accepted the message request",
        )
        .into_owned(),
        SystemEvent::Call(line) if line.group => group_call_text(line, names),
        SystemEvent::Call(line) => call_text(line),
        SystemEvent::Joined => trc("@info:status chat notice", "You joined the group").into_owned(),
        SystemEvent::NumberChanged { user } => trfc(
            "@info:status chat notice, %1 a name",
            "%1 changed their phone number",
            &[&names.display_name(user)],
        ),
    }
}

/// The line of a 1:1 call: what the call is or became, and voice or video when a signal of
/// the call said which. Kourier shows calls and does not take them: a call that rings, rings
/// on the user's phone.
fn call_text(line: &CallLine) -> String {
    use CallDirection::{Incoming, Outgoing};
    use CallMedia::{Video, Voice};
    use CallState::{Answered, Declined, Missed, Ringing, Started, Unanswered};
    let text = match (line.direction, line.state, line.media) {
        (Incoming, Ringing { .. } | Started, Some(Voice)) => trc(
            "@info:status chat notice, a call that rings on the user's phone right now",
            "Incoming voice call…",
        ),
        (Incoming, Ringing { .. } | Started, Some(Video)) => trc(
            "@info:status chat notice, a call that rings on the user's phone right now",
            "Incoming video call…",
        ),
        (Incoming, Ringing { .. } | Started, None) => trc(
            "@info:status chat notice, a call that rings on the user's phone right now",
            "Incoming call…",
        ),
        (Incoming, Missed | Unanswered, Some(Voice)) => trc(
            "@info:status chat notice, a call nobody answered",
            "Missed voice call",
        ),
        (Incoming, Missed | Unanswered, Some(Video)) => trc(
            "@info:status chat notice, a call nobody answered",
            "Missed video call",
        ),
        (Incoming, Missed | Unanswered, None) => trc(
            "@info:status chat notice, a call nobody answered",
            "Missed call",
        ),
        (_, Declined, Some(Voice)) => trc(
            "@info:status chat notice, a call that was declined",
            "Declined voice call",
        ),
        (_, Declined, Some(Video)) => trc(
            "@info:status chat notice, a call that was declined",
            "Declined video call",
        ),
        (_, Declined, None) => trc(
            "@info:status chat notice, a call that was declined",
            "Declined call",
        ),
        (Incoming, Answered, Some(Voice)) => trc(
            "@info:status chat notice, a call the user took on their phone",
            "Voice call · answered on another device",
        ),
        (Incoming, Answered, Some(Video)) => trc(
            "@info:status chat notice, a call the user took on their phone",
            "Video call · answered on another device",
        ),
        (Incoming, Answered, None) => trc(
            "@info:status chat notice, a call the user took on their phone",
            "Call · answered on another device",
        ),
        (Outgoing, Missed | Unanswered, Some(Voice)) => trc(
            "@info:status chat notice, a call the user made that nobody answered",
            "Unanswered voice call",
        ),
        (Outgoing, Missed | Unanswered, Some(Video)) => trc(
            "@info:status chat notice, a call the user made that nobody answered",
            "Unanswered video call",
        ),
        (Outgoing, Missed | Unanswered, None) => trc(
            "@info:status chat notice, a call the user made that nobody answered",
            "Unanswered call",
        ),
        (Outgoing, Answered | Ringing { .. } | Started, Some(Voice)) => trc(
            "@info:status chat notice, a call the user made from their phone",
            "Outgoing voice call",
        ),
        (Outgoing, Answered | Ringing { .. } | Started, Some(Video)) => trc(
            "@info:status chat notice, a call the user made from their phone",
            "Outgoing video call",
        ),
        (Outgoing, Answered | Ringing { .. } | Started, None) => trc(
            "@info:status chat notice, a call the user made from their phone",
            "Outgoing call",
        ),
    };
    text.into_owned()
}

/// The line of a group call. A group call has no voice or video kind; who rang or started
/// it is named when a signal said (the phone's own sync names nobody).
fn group_call_text(line: &CallLine, names: &dyn Names) -> String {
    use CallDirection::{Incoming, Outgoing};
    use CallState::{Answered, Declined, Missed, Ringing, Started, Unanswered};
    let text = match (line.direction, line.state, &line.by) {
        (Incoming, Ringing { .. }, Some(by)) => {
            return trfc(
                "@info:status chat notice, a group call that rings on the user's phone right now, %1 who rings",
                "Incoming group call from %1…",
                &[&names.display_name(by)],
            );
        }
        (Incoming, Ringing { .. }, None) => trc(
            "@info:status chat notice, a group call that rings on the user's phone right now",
            "Incoming group call…",
        ),
        (Incoming, Started, Some(by)) => {
            return trfc(
                "@info:status chat notice, %1 who",
                "%1 started a group call",
                &[&names.display_name(by)],
            );
        }
        (Incoming, Started, None) => trc(
            "@info:status chat notice, nobody is known to have started it",
            "A group call was started",
        ),
        (Outgoing, Started | Ringing { .. }, _) => trc(
            "@info:status chat notice, the user did it",
            "You started a group call",
        ),
        (Incoming, Missed | Unanswered, _) => trc(
            "@info:status chat notice, a call nobody answered",
            "Missed group call",
        ),
        (Outgoing, Missed | Unanswered, _) => trc(
            "@info:status chat notice, a call the user made that nobody answered",
            "Unanswered group call",
        ),
        (_, Declined, _) => trc(
            "@info:status chat notice, a call that was declined",
            "Declined group call",
        ),
        (Incoming, Answered, _) => trc(
            "@info:status chat notice, a group call the user joined on their phone",
            "Group call · joined on another device",
        ),
        (Outgoing, Answered, _) => trc(
            "@info:status chat notice, a call the user made from their phone",
            "Outgoing group call",
        ),
    };
    text.into_owned()
}

/// Icon of a call line, by what the call is or became.
fn call_icon(line: &CallLine) -> &'static str {
    match (line.direction, line.state) {
        (_, CallState::Declined) => "call-stop",
        (CallDirection::Incoming, CallState::Ringing { .. }) => "call-incoming",
        (CallDirection::Incoming, CallState::Missed | CallState::Unanswered) => "call-missed",
        (CallDirection::Incoming, CallState::Started | CallState::Answered) => "call-start",
        (CallDirection::Outgoing, _) => "call-outgoing",
    }
}

/// How a notice row looks beyond its text and icon.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NoticeTone {
    /// Like every other notice.
    #[default]
    Plain,
    /// A call that rings right now: the row looks alive until its state changes.
    Ringing,
    /// Something the user did not get or turned down (a missed or declined call): the theme's
    /// negative text colour.
    Negative,
}

impl NoticeTone {
    /// Stable name for the front-end (`""`, `ringing`, `negative`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "",
            Self::Ringing => "ringing",
            Self::Negative => "negative",
        }
    }
}

/// The tone of a system row. Only a call line has one: ringing is alive, an incoming call
/// nobody answered and a declined call are negative (as Signal Desktop colours them), a call
/// the user made that went unanswered and every other state are plain.
pub fn system_tone(event: &SystemEvent) -> NoticeTone {
    match event {
        SystemEvent::Call(line) => match (line.direction, line.state) {
            (_, CallState::Ringing { .. }) => NoticeTone::Ringing,
            (_, CallState::Declined) => NoticeTone::Negative,
            (CallDirection::Incoming, CallState::Missed | CallState::Unanswered) => {
                NoticeTone::Negative
            }
            _ => NoticeTone::Plain,
        },
        _ => NoticeTone::Plain,
    }
}

/// Icon of a system row.
pub fn system_icon(event: &SystemEvent) -> &'static str {
    match event {
        SystemEvent::MembersAdded { .. } | SystemEvent::Joined => "list-add-user",
        SystemEvent::MembersRemoved { .. } | SystemEvent::MemberLeft { .. } => "list-remove-user",
        SystemEvent::TitleChanged { .. } | SystemEvent::DescriptionChanged { .. } => {
            "document-edit"
        }
        SystemEvent::AvatarChanged { .. } => "photo",
        SystemEvent::TimerChanged { .. } => "clock",
        SystemEvent::IdentityKeyChanged { .. } | SystemEvent::SessionRefreshed { .. } => {
            "security-high"
        }
        SystemEvent::GroupCreated { .. } => "user-group-new",
        SystemEvent::MessageRequestAccepted => "dialog-ok",
        SystemEvent::Call(line) => call_icon(line),
        SystemEvent::NumberChanged { .. } => "phone",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names::NameMap;
    use kourier_model::{Attachment, RangeKind, TextRange};

    fn names() -> (NameMap, UserId, UserId) {
        let mut m = NameMap::new();
        let a = UserId::parse("aci:a").expect("id");
        let b = UserId::parse("aci:b").expect("id");
        m.insert(a.clone(), "Olena");
        m.insert(b.clone(), "Taras");
        (m, a, b)
    }

    #[test]
    fn text_previews_are_one_line_with_hidden_spoilers() {
        let (n, ..) = names();
        let t = Text {
            body: "the answer\nis 42 ok".into(),
            ranges: vec![TextRange {
                start: 14,
                len: 2,
                kind: RangeKind::Spoiler,
            }],
        };
        assert_eq!(text_line(&t, &n), "the answer is ■■■ ok");
        let long = "x".repeat(400);
        let s = body_summary(&Body::text(long), &n);
        assert_eq!(s.text.chars().count(), PREVIEW_MAX_CHARS + 1);
        assert!(s.text.ends_with('…'));
    }

    #[test]
    fn media_previews() {
        let (n, ..) = names();
        let img = Attachment {
            kind: AttachmentKind::Image,
            ..Default::default()
        };
        let media = |atts: Vec<Attachment>, caption: Option<&str>| Body::Media {
            attachments: atts,
            caption: caption.map(Text::plain),
        };
        assert_eq!(
            body_summary(&media(vec![img.clone()], None), &n),
            Summary::new("Photo", "photo")
        );
        assert_eq!(
            body_summary(&media(vec![img.clone(), img.clone()], None), &n).text,
            "2 photos"
        );
        assert_eq!(
            body_summary(&media(vec![img.clone()], Some("sunset")), &n).text,
            "sunset"
        );
        let file = Attachment {
            kind: AttachmentKind::File,
            file_name: Some("report.pdf".into()),
            ..Default::default()
        };
        assert_eq!(
            body_summary(&media(vec![file], None), &n),
            Summary::new("report.pdf", "mail-attachment")
        );
    }

    #[test]
    fn system_texts() {
        let (n, a, b) = names();
        let added = SystemEvent::MembersAdded {
            by: Some(a.clone()),
            users: vec![b.clone()],
        };
        assert_eq!(system_text(&added, &n), "Olena added Taras");
        let three = SystemEvent::MembersAdded {
            by: None,
            users: vec![a.clone(), b.clone(), a.clone()],
        };
        assert_eq!(system_text(&three, &n), "You added Olena, Taras and Olena");
        let timer = SystemEvent::TimerChanged {
            by: Some(b),
            timer: Some(Duration::from_secs(7 * 86_400)),
        };
        assert_eq!(
            system_text(&timer, &n),
            "Taras set the disappearing message timer to 1 week"
        );
        let title = SystemEvent::TitleChanged {
            by: None,
            title: "FPV\nSquad".into(),
        };
        assert_eq!(
            system_text(&title, &n),
            "You changed the group name to “FPV Squad”"
        );
        // A time that is no whole number of one unit (a custom timer) is spelled out.
        assert_eq!(timer_label(Duration::from_secs(45)), "45 seconds");
        assert_eq!(timer_label(Duration::from_secs(90)), "1 minute 30 seconds");
        assert_eq!(
            timer_label(Duration::from_secs(3661)),
            "1 hour 1 minute 1 second"
        );
        assert_eq!(timer_label(Duration::from_secs(5400)), "90 minutes");
        assert_eq!(timer_label(Duration::from_secs(2 * 604_800)), "2 weeks");
        assert_eq!(timer_label(Duration::from_secs(3600 * 8)), "8 hours");
    }

    /// A call line: every state has a text and an icon, a 1:1 line says voice or video when
    /// a signal said, a group line names who rang or started it when a signal said.
    #[test]
    fn call_line_texts_and_icons() {
        use CallDirection::{Incoming, Outgoing};
        let (n, olena, _) = names();
        let line = |direction, group, state, media: Option<CallMedia>, by: Option<&UserId>| {
            let mut l = CallLine::signal(direction, group, state, 5);
            if let Some(m) = media {
                l = l.with_media(m, 5);
            }
            if let Some(u) = by {
                l = l.with_by(u.clone(), 5);
            }
            SystemEvent::Call(l)
        };
        let ringing = CallState::Ringing { deadline: 9 };
        let voice = Some(CallMedia::Voice);
        let video = Some(CallMedia::Video);
        let cases: Vec<(SystemEvent, &str, &str)> = vec![
            (
                line(Incoming, false, ringing, voice, Some(&olena)),
                "Incoming voice call…",
                "call-incoming",
            ),
            (
                line(Incoming, false, ringing, video, Some(&olena)),
                "Incoming video call…",
                "call-incoming",
            ),
            (
                line(Incoming, false, CallState::Missed, None, Some(&olena)),
                "Missed call",
                "call-missed",
            ),
            (
                line(Incoming, false, CallState::Missed, video, Some(&olena)),
                "Missed video call",
                "call-missed",
            ),
            (
                line(Incoming, false, CallState::Declined, voice, Some(&olena)),
                "Declined voice call",
                "call-stop",
            ),
            (
                line(Incoming, false, CallState::Answered, voice, Some(&olena)),
                "Voice call · answered on another device",
                "call-start",
            ),
            (
                line(Outgoing, false, CallState::Answered, video, None),
                "Outgoing video call",
                "call-outgoing",
            ),
            (
                line(Outgoing, false, CallState::Unanswered, voice, None),
                "Unanswered voice call",
                "call-outgoing",
            ),
            (
                line(Incoming, true, ringing, None, Some(&olena)),
                "Incoming group call from Olena…",
                "call-incoming",
            ),
            (
                line(Incoming, true, ringing, None, None),
                "Incoming group call…",
                "call-incoming",
            ),
            (
                line(Incoming, true, CallState::Missed, None, Some(&olena)),
                "Missed group call",
                "call-missed",
            ),
            (
                line(Incoming, true, CallState::Started, None, Some(&olena)),
                "Olena started a group call",
                "call-start",
            ),
            (
                line(Incoming, true, CallState::Started, None, None),
                "A group call was started",
                "call-start",
            ),
            (
                line(Outgoing, true, CallState::Started, None, None),
                "You started a group call",
                "call-outgoing",
            ),
            (
                line(Incoming, true, CallState::Answered, None, None),
                "Group call · joined on another device",
                "call-start",
            ),
            (
                line(Incoming, true, CallState::Declined, None, None),
                "Declined group call",
                "call-stop",
            ),
            (
                line(Outgoing, true, CallState::Answered, None, None),
                "Outgoing group call",
                "call-outgoing",
            ),
        ];
        for (event, text, icon) in cases {
            assert_eq!(system_text(&event, &n), text);
            assert_eq!(system_icon(&event), icon, "{text}");
            // The list's preview and a notification's line say the same.
            assert_eq!(body_summary(&Body::System(event), &n).text, text);
        }
    }

    /// The tone of a call line: alive while it rings, negative for a call nobody answered
    /// that came in and for a declined one, plain for the rest (also for an unanswered call
    /// the user made, and for every other notice).
    #[test]
    fn call_tones() {
        use CallDirection::{Incoming, Outgoing};
        let state = |direction, group, state| {
            NoticeTone::as_str(system_tone(&SystemEvent::Call(CallLine::signal(
                direction, group, state, 5,
            ))))
        };
        let ringing = CallState::Ringing { deadline: 9 };
        for group in [false, true] {
            assert_eq!(state(Incoming, group, ringing), "ringing");
            assert_eq!(state(Incoming, group, CallState::Missed), "negative");
            assert_eq!(state(Incoming, group, CallState::Declined), "negative");
            assert_eq!(state(Incoming, group, CallState::Answered), "");
            assert_eq!(state(Incoming, group, CallState::Started), "");
            assert_eq!(state(Outgoing, group, CallState::Unanswered), "");
            assert_eq!(state(Outgoing, group, CallState::Answered), "");
            assert_eq!(state(Outgoing, group, CallState::Started), "");
        }
        assert_eq!(system_tone(&SystemEvent::Joined), NoticeTone::Plain);
        assert_eq!(
            system_tone(&SystemEvent::MessageRequestAccepted),
            NoticeTone::Plain
        );
    }

    #[test]
    fn snapshots() {
        let (n, ..) = names();
        let snap = QuoteSnapshot {
            text: None,
            attachment: Some(AttachmentKind::Voice),
            file_name: None,
            ..QuoteSnapshot::default()
        };
        assert_eq!(snapshot_summary(&snap, &n).text, "Voice message");
        assert_eq!(
            snapshot_summary(&QuoteSnapshot::default(), &n).text,
            "Original message not found"
        );
    }
}
