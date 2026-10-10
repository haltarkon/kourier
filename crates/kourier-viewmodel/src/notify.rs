//! Texts of desktop notifications and the tray (§8.7, §8.8): conversation titles, the one-line
//! message of a notification, the catch-up summary, account labels, the unread tooltip.
//!
//! `kourier-qt` hands these to the desktop integration (`kourier_app::desktop::Presenter`).
//! Log safety (§9.1): the results are names and message texts; never log them.

use kourier_core::facade::{AccountRow, ConversationDetail, ConversationRow, MessageRow};
use kourier_model::{ConvKind, UserId};

use crate::conversations::network_name;
use crate::i18n::{trc, trfc, trnc};
use crate::names::{NameMap, fallback_name, first_name, single_line};
use crate::preview::{body_summary, deleted_text};

/// Longest message line in a notification (characters).
pub const LINE_MAX_CHARS: usize = 300;

/// The title of a conversation (`Note to Self`, the group or contact name, or a fallback for
/// nameless chats), the same as the conversation list shows.
pub fn conversation_title(row: &ConversationRow) -> String {
    crate::core_source::conversation_title(row)
}

/// Whether a title is a placeholder without a name ("Signal user ⋯1f2e", a phone number): its
/// avatar shows a person glyph instead of initials.
pub fn is_anonymous_title(title: &str) -> bool {
    title.contains('\u{22EF}')
        || (!title.is_empty()
            && title
                .chars()
                .all(|c| c.is_ascii_digit() || " +()-".contains(c)))
}

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", s[..i].trim_end()),
        None => s.to_owned(),
    }
}

fn author_name(msg: &MessageRow) -> Option<String> {
    let author: &UserId = msg.author.as_ref()?;
    Some(
        msg.author_name
            .as_ref()
            .map(|n| single_line(n.expose()))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| fallback_name(author)),
    )
}

/// The notification line of a message: its text (mentions as `@Name`, spoilers hidden) or a
/// media label, prefixed with the author's first name in groups ("Ari: see you").
pub fn message_line(detail: &ConversationDetail, msg: &MessageRow) -> String {
    let mut names = NameMap::new();
    if let (Some(author), Some(name)) = (&msg.author, author_name(msg)) {
        names.insert(author.clone(), name);
    }
    // A call line has no author; who rang or started the call is named in the line, and the
    // core gives their name with the row ("Olena started a group call").
    if let Some(by) = msg.body.call_line().and_then(|line| line.by.as_ref())
        && let Some(name) = msg.author_name.as_ref().map(|n| single_line(n.expose()))
        && !name.is_empty()
    {
        names.insert(by.clone(), name);
    }
    let text = match &msg.deleted {
        Some(d) => deleted_text(d, msg.author.is_none()),
        None => body_summary(&msg.body, &names).text,
    };
    let text = clip(&single_line(&text), LINE_MAX_CHARS);
    let group = detail.row.kind != ConvKind::Direct;
    match author_name(msg) {
        Some(author) if group => trfc(
            "@info:notification, %1 the first name of the sender, %2 the text of the message",
            "%1: %2",
            &[first_name(&author), &text],
        ),
        _ => text,
    }
}

/// "3 new messages".
pub fn new_messages(count: usize) -> String {
    trnc(
        "@info:notification, %1 how many messages",
        "%1 new message",
        "%1 new messages",
        u64::try_from(count).unwrap_or(u64::MAX),
    )
}

/// The catch-up summary: "12 new messages in 4 chats".
pub fn catch_up(messages: u32, conversations: u32) -> String {
    let m = trnc(
        "@info:notification summary, part of a sentence, %1 how many messages",
        "%1 new message",
        "%1 new messages",
        u64::from(messages),
    );
    let c = trnc(
        "@info:notification summary, part of a sentence, %1 how many chats",
        "%1 chat",
        "%1 chats",
        u64::from(conversations),
    );
    trfc(
        "@info:notification summary, %1 how many new messages, %2 how many chats",
        "%1 in %2",
        &[&m, &c],
    )
}

/// The account's label in a notification when several accounts exist: its profile name, or
/// the network name.
pub fn account_label(account: &AccountRow) -> String {
    account
        .display_name
        .as_deref()
        .map(single_line)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| network_name(account.network).to_owned())
}

/// The tray tooltip: "3 unread messages" (empty without unread messages).
pub fn unread_tooltip(unread: u32) -> String {
    match unread {
        0 => String::new(),
        n => trnc(
            "@info:tooltip tray icon, %1 how many unread messages",
            "%1 unread message",
            "%1 unread messages",
            u64::from(n),
        ),
    }
}

/// Fixed strings: application name, "New message", the reply placeholder, the tray entries.
pub struct FixedTexts {
    /// "Kourier".
    pub app_name: String,
    /// "New message".
    pub new_message: String,
    /// "Reply…".
    pub reply_placeholder: String,
    /// "Show Kourier".
    pub tray_show: String,
    /// "Quit".
    pub tray_quit: String,
}

/// The fixed strings, translated.
pub fn fixed_texts() -> FixedTexts {
    FixedTexts {
        // TRANSLATORS: the name of the application; most languages keep it.
        app_name: trc("@title the name of the application", "Kourier").into_owned(),
        new_message: trc(
            "@info:notification text of a notification that hides its content",
            "New message",
        )
        .into_owned(),
        reply_placeholder: trc(
            "@info:placeholder in the reply field of a notification",
            "Reply…",
        )
        .into_owned(),
        tray_show: trc("@action:inmenu tray icon", "Show Kourier").into_owned(),
        tray_quit: trc("@action:inmenu tray icon", "Quit").into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kourier_core::facade::OriginKind;
    use kourier_model::{
        AccountId, AccountState, AccountUuid, Body, ConnState, ConversationFeatures,
        ConversationId, Deletion, MessageId, NetworkKind, Redacted, Text,
    };

    use super::*;

    fn row(kind: ConvKind, title: Option<&str>, peer: Option<UserId>) -> ConversationRow {
        ConversationRow {
            id: ConversationId(1),
            account: AccountId(1),
            kind,
            is_self: false,
            title: title.map(str::to_owned),
            peer,
            avatar: None,
            preview: None,
            last_activity: 0,
            unread: 1,
            mentions: 0,
            marked_unread: false,
            muted_until: None,
            local_muted_until: None,
            archived: false,
            pinned_order: None,
            blocked: false,
            request_pending: false,
            features_hash: 0,
            has_draft: false,
            hidden: false,
        }
    }

    fn detail(kind: ConvKind) -> ConversationDetail {
        ConversationDetail {
            row: row(kind, Some("Weekend"), None),
            members: Vec::new(),
            features: ConversationFeatures::default(),
            timer: None::<Duration>,
            draft: None,
        }
    }

    fn msg(body: Body, author: Option<&str>, deleted: bool) -> MessageRow {
        let user = UserId::parse("aci:00000000-0000-4000-8000-0000000a11ce").expect("id");
        MessageRow {
            id: MessageId(1),
            sort_key: 0,
            author: Some(user),
            author_name: author.map(|a| Redacted::new(a.to_owned())),
            author_avatar: None,
            origin: OriginKind::Remote,
            device: None,
            body,
            reply: None,
            sent_at: 0,
            server_at: None,
            received_at: 0,
            revision: 0,
            edited_at: None,
            deleted: deleted.then_some(Deletion::ForEveryone { by: None }),
            out_state: None,
            read_state: None,
            reactions: Vec::new(),
            expiry: None,
            view_once: false,
            history: false,
            mentions_me: false,
            actions: Default::default(),
        }
    }

    /// The notification of a group call names who rang or started it: the row has no author,
    /// the core gives the name of the line's `by` with it.
    #[test]
    fn a_call_line_names_who_started_it() {
        use kourier_model::{CallDirection, CallLine, CallState, SystemEvent};
        let olena = UserId::parse("aci:00000000-0000-4000-8000-0000000a11ce").expect("id");
        let line = |state: CallState| {
            let mut m = msg(
                Body::System(SystemEvent::Call(
                    CallLine::signal(CallDirection::Incoming, true, state, 5)
                        .with_by(olena.clone(), 5),
                )),
                Some("Olena Kestrel\nMD"),
                false,
            );
            // A call line has no author.
            m.author = None;
            m
        };
        let group = detail(ConvKind::Group);
        assert_eq!(
            message_line(&group, &line(CallState::Started)),
            "Olena Kestrel MD started a group call"
        );
        assert_eq!(
            message_line(&group, &line(CallState::Missed)),
            crate::preview::body_summary(&line(CallState::Missed).body, &NameMap::new()).text,
            "a missed group call names nobody"
        );
        // Without a name from the core: the fallback label, as before.
        let mut unnamed = line(CallState::Started);
        unnamed.author_name = None;
        let text = message_line(&group, &unnamed);
        assert!(
            text.ends_with("started a group call") && !text.contains("Olena"),
            "{text}"
        );
    }

    #[test]
    fn lines() {
        let text = Body::Text(Text::plain("see you\nat 8"));
        assert_eq!(
            message_line(
                &detail(ConvKind::Direct),
                &msg(text.clone(), Some("Ari Kestrel"), false)
            ),
            "see you at 8"
        );
        assert_eq!(
            message_line(
                &detail(ConvKind::Group),
                &msg(text, Some("Ari Kestrel"), false)
            ),
            "Ari: see you at 8"
        );
        let long = Body::Text(Text::plain("x".repeat(500)));
        assert!(
            message_line(&detail(ConvKind::Direct), &msg(long, None, false))
                .chars()
                .count()
                <= LINE_MAX_CHARS + 1
        );
        let deleted = message_line(
            &detail(ConvKind::Direct),
            &msg(Body::Text(Text::plain("gone")), None, true),
        );
        assert!(!deleted.contains("gone"));
        // A nameless author in a group: the fallback label.
        let anon = message_line(
            &detail(ConvKind::Group),
            &msg(Body::Text(Text::plain("hi")), None, false),
        );
        assert!(
            anon.ends_with(": hi") && anon.starts_with("Signal"),
            "{anon}"
        );
    }

    #[test]
    fn titles() {
        assert_eq!(
            conversation_title(&row(ConvKind::Group, Some("FPV"), None)),
            "FPV"
        );
        let peer = UserId::parse("aci:00000000-0000-4000-8000-0000000a11ce").expect("id");
        let t = conversation_title(&row(ConvKind::Direct, None, Some(peer)));
        assert!(is_anonymous_title(&t), "{t}");
        assert!(is_anonymous_title("+49 151 2345"));
        assert!(!is_anonymous_title("Olena"));
    }

    #[test]
    fn counts() {
        assert_eq!(new_messages(1), "1 new message");
        assert_eq!(new_messages(3), "3 new messages");
        assert_eq!(catch_up(12, 4), "12 new messages in 4 chats");
        assert_eq!(catch_up(1, 1), "1 new message in 1 chat");
        assert_eq!(unread_tooltip(0), "");
        assert_eq!(unread_tooltip(2), "2 unread messages");
    }

    #[test]
    fn account_labels() {
        let mut a = AccountRow {
            id: AccountId(1),
            uuid: AccountUuid::new_random(),
            network: NetworkKind::Signal,
            state: AccountState::Active,
            display_name: Some("Work".into()),
            conn: ConnState::Connecting,
            own_device: None,
            avatar: None,
            number: None,
        };
        assert_eq!(account_label(&a), "Work");
        a.display_name = None;
        assert_eq!(account_label(&a), "Signal");
    }
}
