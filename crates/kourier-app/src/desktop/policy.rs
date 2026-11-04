//! The desktop side of the notification policy (§8.7), free of D-Bus so it can be unit-tested:
//! which notification replaces which, when one closes, what a click or a reply means, and what
//! a notification shows in each [`NotificationContent`] mode.
//!
//! The core decides *whether* a message notifies (incoming, unread, not muted, not catch-up, not
//! the focused conversation; `UiEvent::Notify`) and when a catch-up summary is due
//! (`UiEvent::CatchUpSummary`). This module keeps one notification per conversation (replaced
//! by every new message) and one summary per account, and closes them when the conversation is
//! read (here or on another device), deleted, or one of its shown messages is deleted or
//! edited, or was a missed call that turned out answered or declined.

use std::collections::{HashMap, VecDeque};

use kourier_core::facade::{ConversationRow, MessageRow, Notification as CoreNotification};
use kourier_desktop::notifications::{CloseReason, ImageData, InlineReply, Notification, Urgency};
use kourier_model::{AccountId, ConversationId, MessageId};

use crate::prefs::NotificationContent;

/// The notification `category` hint of a received message.
pub const CATEGORY_MESSAGE: &str = "im.received";
/// The category of the catch-up summary.
pub const CATEGORY_SUMMARY: &str = "im";

/// What a notification is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// New messages of one conversation.
    Conversation(ConversationId),
    /// The catch-up summary of one account.
    Summary(AccountId),
}

/// A click on a notification: raise the window with `token` and open the conversation.
#[derive(Clone, PartialEq, Eq)]
pub struct Activation {
    /// What was clicked.
    pub target: Target,
    /// The XDG activation token the server sent before the click.
    pub token: Option<String>,
}

impl std::fmt::Debug for Activation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Activation")
            .field("target", &self.target)
            .field("token", &self.token.is_some())
            .finish()
    }
}

#[derive(Clone, Debug)]
struct Shown {
    id: u32,
    account: AccountId,
    /// The messages this notification announced, with the revision shown.
    messages: Vec<(MessageId, u16)>,
    /// The content mode it is shown in now: `NameAndMessage` means the text of a message is on
    /// screen (a chat that becomes protected takes it off again).
    mode: NotificationContent,
}

/// Most messages remembered per conversation notification (older ones cannot be told apart
/// from the newest anyway).
const MAX_TRACKED: usize = 32;

/// Most ids of expired notifications that showed text, all chats together, remembered for this
/// run (the oldest go first).
const MAX_EXPIRED_TEXT: usize = 64;

/// Which notifications are on screen and what they stand for.
#[derive(Debug, Default)]
pub struct NotificationBook {
    convs: HashMap<ConversationId, Shown>,
    summaries: HashMap<AccountId, u32>,
    targets: HashMap<u32, Target>,
    tokens: HashMap<u32, String>,
    /// Notifications that EXPIRED while they showed a message's text, as (chat, id), oldest
    /// first: a server may keep such a notification in its history (Plasma does), and a chat
    /// that becomes protected has them revoked. This run only: ids mean nothing to a restarted
    /// server (nor to a later run of Kourier; architecture §8.7).
    expired_text: VecDeque<(ConversationId, u32)>,
}

impl NotificationBook {
    /// The id a new message of `conv` replaces (0 = none shown).
    pub fn replaces_for(&self, conv: ConversationId) -> u32 {
        self.convs.get(&conv).map_or(0, |s| s.id)
    }

    /// How many messages the notification of `conv` announced so far.
    pub fn count(&self, conv: ConversationId) -> usize {
        self.convs.get(&conv).map_or(0, |s| s.messages.len())
    }

    /// Records that `id` now shows `message` (revision `revision`) of `conv`, in the content
    /// mode `mode`.
    pub fn shown_message(
        &mut self,
        conv: ConversationId,
        account: AccountId,
        message: MessageId,
        revision: u16,
        id: u32,
        mode: NotificationContent,
    ) {
        let entry = self.convs.entry(conv).or_insert_with(|| Shown {
            id,
            account,
            messages: Vec::new(),
            mode,
        });
        entry.mode = mode;
        if entry.id != id {
            // The server gave a new id (the old one was closed meanwhile).
            self.targets.remove(&entry.id);
            self.tokens.remove(&entry.id);
            entry.id = id;
        }
        if !entry.messages.iter().any(|(m, _)| *m == message) {
            entry.messages.push((message, revision));
            if entry.messages.len() > MAX_TRACKED {
                entry.messages.remove(0);
            }
        }
        self.targets.insert(id, Target::Conversation(conv));
    }

    /// The content mode the notification of `conv` is shown in (`None` = none shown).
    pub fn shown_in(&self, conv: ConversationId) -> Option<NotificationContent> {
        self.convs.get(&conv).map(|s| s.mode)
    }

    /// Records that the notification of `conv` was shown again, in `mode`, as `id` (its own id
    /// when the server replaced it in place, else the new one the server gave).
    pub fn reshown(&mut self, conv: ConversationId, id: u32, mode: NotificationContent) {
        let Some(entry) = self.convs.get_mut(&conv) else {
            return;
        };
        entry.mode = mode;
        if entry.id != id {
            self.targets.remove(&entry.id);
            self.tokens.remove(&entry.id);
            entry.id = id;
        }
        self.targets.insert(id, Target::Conversation(conv));
    }

    /// Whether a notification of `conv` that showed text expired in this run (and was not
    /// revoked yet).
    pub fn has_expired_text(&self, conv: ConversationId) -> bool {
        self.expired_text.iter().any(|(c, _)| *c == conv)
    }

    /// The chats with a remembered expired notification that showed text (each once).
    pub fn expired_chats(&self) -> Vec<ConversationId> {
        let mut chats: Vec<ConversationId> = Vec::new();
        for (c, _) in &self.expired_text {
            if !chats.contains(c) {
                chats.push(*c);
            }
        }
        chats
    }

    /// Takes the ids of `conv`'s expired notifications that showed text: they are to be revoked.
    pub fn take_expired_text(&mut self, conv: ConversationId) -> Vec<u32> {
        let ids = self
            .expired_text
            .iter()
            .filter(|(c, _)| *c == conv)
            .map(|(_, id)| *id)
            .collect();
        self.expired_text.retain(|(c, _)| *c != conv);
        ids
    }

    /// The id the summary of `account` replaces (0 = none shown).
    pub fn summary_replaces(&self, account: AccountId) -> u32 {
        self.summaries.get(&account).copied().unwrap_or(0)
    }

    /// Records the summary of `account`.
    pub fn shown_summary(&mut self, account: AccountId, id: u32) {
        if let Some(old) = self.summaries.insert(account, id)
            && old != id
        {
            self.targets.remove(&old);
            self.tokens.remove(&old);
        }
        self.targets.insert(id, Target::Summary(account));
    }

    /// Conversations with a notification on screen.
    pub fn conversations(&self) -> Vec<ConversationId> {
        self.convs.keys().copied().collect()
    }

    /// Whether `conv` has a notification on screen.
    pub fn is_tracked(&self, conv: ConversationId) -> bool {
        self.convs.contains_key(&conv)
    }

    /// The messages announced for `conv`, with the revision shown.
    pub fn messages(&self, conv: ConversationId) -> Vec<(MessageId, u16)> {
        self.convs
            .get(&conv)
            .map(|s| s.messages.clone())
            .unwrap_or_default()
    }

    /// Forgets that `conv`'s notification stands for `message` (the notification stays):
    /// a call line among several announced messages that moved on. Returns whether the
    /// notification stands for nothing any more.
    pub fn forget_message(&mut self, conv: ConversationId, message: MessageId) -> bool {
        match self.convs.get_mut(&conv) {
            Some(shown) => {
                shown.messages.retain(|(m, _)| *m != message);
                shown.messages.is_empty()
            }
            None => false,
        }
    }

    /// Forgets the ids of `conv`'s expired notifications (the chat is gone: `ConversationsRemoved`).
    pub fn forget_expired_text(&mut self, conv: ConversationId) {
        self.expired_text.retain(|(c, _)| *c != conv);
    }

    /// Forgets `conv`'s notification; returns the id to close.
    pub fn close_conversation(&mut self, conv: ConversationId) -> Option<u32> {
        let shown = self.convs.remove(&conv)?;
        self.targets.remove(&shown.id);
        self.tokens.remove(&shown.id);
        Some(shown.id)
    }

    /// Forgets every notification of `account`; returns the ids to close.
    pub fn close_account(&mut self, account: AccountId) -> Vec<u32> {
        let convs: Vec<ConversationId> = self
            .convs
            .iter()
            .filter(|(_, s)| s.account == account)
            .map(|(c, _)| *c)
            .collect();
        let mut ids: Vec<u32> = convs
            .into_iter()
            .filter_map(|c| self.close_conversation(c))
            .collect();
        if let Some(id) = self.summaries.remove(&account) {
            self.targets.remove(&id);
            self.tokens.remove(&id);
            ids.push(id);
        }
        ids
    }

    /// Forgets every summary (the user looked at the window); returns the ids to close.
    pub fn close_summaries(&mut self) -> Vec<u32> {
        let ids: Vec<u32> = self.summaries.drain().map(|(_, id)| id).collect();
        for id in &ids {
            self.targets.remove(id);
            self.tokens.remove(id);
        }
        ids
    }

    /// Forgets everything; returns every id (shutdown).
    pub fn close_all(&mut self) -> Vec<u32> {
        let ids: Vec<u32> = self.targets.keys().copied().collect();
        *self = Self::default();
        ids
    }

    /// The server closed `id` (expired, dismissed or by us). A conversation notification that
    /// EXPIRED while it showed text is remembered ([`Self::take_expired_text`]): the server may
    /// keep it in its history, and only an expired one is kept there (Plasma's history drops one
    /// the user dismissed or the client closed).
    pub fn on_closed(&mut self, id: u32, reason: CloseReason) {
        match self.targets.remove(&id) {
            Some(Target::Conversation(c)) if self.convs.get(&c).is_some_and(|s| s.id == id) => {
                let shown = self.convs.remove(&c);
                if reason == CloseReason::Expired
                    && shown.is_some_and(|s| s.mode == NotificationContent::NameAndMessage)
                {
                    self.expired_text.push_back((c, id));
                    if self.expired_text.len() > MAX_EXPIRED_TEXT {
                        self.expired_text.pop_front();
                    }
                }
            }
            Some(Target::Summary(a)) if self.summaries.get(&a) == Some(&id) => {
                self.summaries.remove(&a);
            }
            _ => {}
        }
        self.tokens.remove(&id);
    }

    /// The server sent an activation token for `id` (before the action).
    pub fn on_token(&mut self, id: u32, token: String) {
        if self.targets.contains_key(&id) {
            self.tokens.insert(id, token);
        }
    }

    /// An action of `id` was invoked; a click (the default action) activates.
    pub fn on_action(&mut self, id: u32, key: &str) -> Option<Activation> {
        let target = *self.targets.get(&id)?;
        let token = self.tokens.remove(&id);
        (key == kourier_desktop::notifications::DEFAULT_ACTION)
            .then_some(Activation { target, token })
    }

    /// An inline reply was sent from `id`: the conversation to send it to.
    pub fn on_reply(&self, id: u32) -> Option<ConversationId> {
        match self.targets.get(&id)? {
            Target::Conversation(c) => Some(*c),
            Target::Summary(_) => None,
        }
    }
}

/// How long the notification of a conversation rests after it was shown: further messages
/// of that conversation that arrive meanwhile are collected and shown together when the time
/// is over (they replace the one on screen), so a burst updates each conversation's
/// notification a few times a second at most (Signal Desktop debounces too). The first
/// message of a conversation is never held back by it ([`Pace`]).
pub const COALESCE: std::time::Duration = std::time::Duration::from_millis(250);

/// What becomes of a new-message notification when it arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admit<I> {
    /// Show it now: its conversation's notification did not change in the last
    /// [`COALESCE`].
    Now,
    /// Collect it: the conversation's notification was shown or updated a moment ago. It is
    /// due at this instant, together with what else arrived for the conversation until then.
    At(I),
}

/// Paces notifications per conversation (architecture §1.3: a received message is announced
/// within 300 ms; §8.7: a burst does not flicker). The first message of a conversation is
/// shown at once; what follows within [`COALESCE`] of a shown notification is collected and
/// replaces it once. `I` is the clock's instant (tokio's in the service, a plain number in
/// tests).
#[derive(Clone, Debug)]
pub struct Pace<I> {
    /// Until when each conversation's notification rests.
    resting: HashMap<ConversationId, I>,
}

impl<I> Default for Pace<I> {
    fn default() -> Self {
        Self {
            resting: HashMap::new(),
        }
    }
}

impl<I: Copy + Ord + std::ops::Add<std::time::Duration, Output = I>> Pace<I> {
    /// A notification for `conv` arrived at `now`.
    pub fn admit(&mut self, conv: ConversationId, now: I) -> Admit<I> {
        // (Conversations whose rest is over are forgotten: the map holds only the busy ones.)
        self.resting.retain(|_, until| *until > now);
        match self.resting.get(&conv) {
            Some(until) => Admit::At(*until),
            None => Admit::Now,
        }
    }

    /// The notification of `conv` was shown or updated at `now`: it rests from here.
    pub fn shown(&mut self, conv: ConversationId, now: I) {
        self.resting.insert(conv, now + COALESCE);
    }
}

/// One notification to show for a burst: the newest message of a conversation and the earlier
/// ones it stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Burst {
    /// The newest message (its notification is shown).
    pub newest: CoreNotification,
    /// Earlier messages of the same conversation in this burst, oldest first.
    pub earlier: Vec<MessageId>,
}

/// Groups collected `Notify` events by conversation, in the order conversations first
/// appeared; repeated message ids count once.
pub fn coalesce(pending: Vec<CoreNotification>) -> Vec<Burst> {
    let mut out: Vec<Burst> = Vec::new();
    for n in pending {
        match out.iter_mut().find(|b| b.newest.conv == n.conv) {
            Some(b) => {
                if b.newest.message != n.message && !b.earlier.contains(&n.message) {
                    b.earlier.push(b.newest.message);
                    b.newest = n;
                }
            }
            None => out.push(Burst {
                newest: n,
                earlier: Vec::new(),
            }),
        }
    }
    out
}

/// Whether a conversation's notification should close: it has been read (here or on another
/// device) and is not marked unread.
pub fn read_now(row: &ConversationRow) -> bool {
    row.unread == 0 && !row.marked_unread
}

/// Whether a notification of this conversation offers an inline reply: the chat takes a new
/// message (the core refuses a send into a chat whose features state a reason it cannot, and
/// the inline reply never looks at the answer, so a reply typed there would be lost), is not
/// blocked and has no message request pending.
pub fn can_reply(row: &ConversationRow) -> bool {
    row.can_send && !row.blocked && !row.request_pending
}

/// Whether a shown message is gone, deleted or edited since it was shown (`shown` revision),
/// or is a call line that moved on ([`call_moved_on`]).
pub fn changed_since_shown(shown: u16, row: Option<&MessageRow>) -> bool {
    match row {
        None => true,
        Some(r) => r.deleted.is_some() || r.revision != shown || call_moved_on(r),
    }
}

/// What becomes of a conversation's notification when one of the messages it stands for is
/// looked at again ([`withdraw`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Withdraw {
    /// Nothing: the message is as it was shown.
    Keep,
    /// The notification stays, but stands for this message no more.
    Forget,
    /// The notification is closed.
    Close,
}

/// What to do with a notification that showed a message at revision `shown`, now that the
/// message is `row`; `newest` says whether it is the newest message the notification stands
/// for (the one whose text it shows).
///
/// A message that is gone, deleted or edited closes it, as ever. A call line that moved on
/// (a missed call that was answered or declined on the phone, or read) closes it only when
/// the notification SHOWS that line: an older call line under a newer message is just not
/// counted any more, and the text of the newer message stays on screen.
pub fn withdraw(shown: u16, row: Option<&MessageRow>, newest: bool) -> Withdraw {
    match row {
        None => Withdraw::Close,
        Some(r) if r.deleted.is_some() || r.revision != shown => Withdraw::Close,
        Some(r) if call_moved_on(r) => {
            if newest {
                Withdraw::Close
            } else {
                Withdraw::Forget
            }
        }
        Some(_) => Withdraw::Keep,
    }
}

/// Whether `row` is a call line that is not unread (any more). A call line is announced
/// while it counts as unread (a missed call, a group call somebody started); when a later
/// signal changes it (answered or declined on the phone, or it rings after all) the core
/// takes its unread mark away, and what was announced is no longer true. The conversation
/// may still hold other unread messages, so [`read_now`] alone does not see it.
pub fn call_moved_on(row: &MessageRow) -> bool {
    row.body.call_line().is_some() && row.read_state != Some(kourier_model::ReadState::Unread)
}

/// Fixed strings of the desktop integration, translated by the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Texts {
    /// The application name ("Kourier").
    pub app_name: String,
    /// The body without content ("New message").
    pub new_message: String,
    /// The inline reply placeholder ("Reply…").
    pub reply_placeholder: String,
    /// The tray's Show entry.
    pub tray_show: String,
    /// The tray's Quit entry.
    pub tray_quit: String,
}

impl Default for Texts {
    fn default() -> Self {
        Self {
            app_name: "Kourier".into(),
            new_message: "New message".into(),
            reply_placeholder: "Reply…".into(),
            tray_show: "Show Kourier".into(),
            tray_quit: "Quit".into(),
        }
    }
}

/// The content mode a conversation's notification is shown in: the user's choice, except that
/// a protected chat ([`ConversationRow::protected`]) never shows the text of a message: its
/// `NameAndMessage` is `NameOnly` (the title and "New message" or "3 new messages"; Threema
/// Desktop empties the body of such a chat's notification and keeps the names in the title,
/// `common/notification.ts` 382-385, `common/dom/ui/notification.ts` 205-237). `Hidden` stays
/// `Hidden`. The inline reply stays where [`can_reply`] offers it: it shows no text (its field
/// has the fixed placeholder, and a reply quotes nothing).
pub fn content_mode(chosen: NotificationContent, row: &ConversationRow) -> NotificationContent {
    match chosen {
        NotificationContent::NameAndMessage if row.protected => NotificationContent::NameOnly,
        other => other,
    }
}

/// The mode a notification that is on screen in `shown` must be shown again in, because its
/// chat became protected after the text was shown (`None` = it may stay as it is): the same
/// rule as [`content_mode`], applied to what is already shown. The notification is shown again
/// under its own id, so the server updates it in place.
pub fn text_to_withdraw(
    shown: NotificationContent,
    row: &ConversationRow,
) -> Option<NotificationContent> {
    let now = content_mode(shown, row);
    (now != shown).then_some(now)
}

/// What one message notification is built from.
#[derive(Clone)]
pub struct MessageContent<'a> {
    /// The content mode.
    pub mode: NotificationContent,
    /// The conversation title (sender or group).
    pub title: &'a str,
    /// The message line ("Ari: see you" in groups).
    pub line: &'a str,
    /// "3 new messages" when the notification announces several (NameOnly mode).
    pub count_line: Option<&'a str>,
    /// The account label when several accounts exist.
    pub origin: Option<&'a str>,
    /// The conversation's avatar.
    pub avatar: Option<ImageData>,
    /// Whether an inline reply is offered (the account can send to this conversation).
    pub can_reply: bool,
    /// The id this notification replaces.
    pub replaces_id: u32,
    /// Fixed strings.
    pub texts: &'a Texts,
}

/// A message notification in the chosen content mode. `Hidden` shows neither names nor text
/// nor avatar nor account and offers no reply; `NameOnly` hides the text.
pub fn message_notification(c: MessageContent<'_>) -> Notification {
    let (summary, body, image, origin, reply) = match c.mode {
        NotificationContent::NameAndMessage => (
            c.title.to_owned(),
            c.line.to_owned(),
            c.avatar,
            c.origin.map(str::to_owned),
            c.can_reply,
        ),
        NotificationContent::NameOnly => (
            c.title.to_owned(),
            c.count_line.unwrap_or(&c.texts.new_message).to_owned(),
            c.avatar,
            c.origin.map(str::to_owned),
            c.can_reply,
        ),
        NotificationContent::Hidden => (
            c.texts.app_name.clone(),
            c.texts.new_message.clone(),
            None,
            None,
            false,
        ),
    };
    Notification {
        replaces_id: c.replaces_id,
        app_name: c.texts.app_name.clone(),
        icon: String::new(),
        summary,
        body,
        default_action: true,
        actions: Vec::new(),
        inline_reply: reply.then(|| InlineReply {
            placeholder: c.texts.reply_placeholder.clone(),
            submit_label: String::new(),
        }),
        desktop_entry: Some(kourier_desktop::APP_ID.to_owned()),
        category: Some(CATEGORY_MESSAGE.to_owned()),
        origin_name: origin,
        image,
        // Never Critical, not even for mentions: critical notifications stay until dismissed
        // and pass Do Not Disturb.
        urgency: Urgency::Normal,
        timeout_ms: -1,
    }
}

/// The catch-up summary ("12 new messages in 4 chats"); counts only, so every mode shows it.
pub fn summary_notification(
    text: &str,
    origin: Option<&str>,
    replaces_id: u32,
    texts: &Texts,
) -> Notification {
    Notification {
        replaces_id,
        app_name: texts.app_name.clone(),
        icon: String::new(),
        summary: texts.app_name.clone(),
        body: text.to_owned(),
        default_action: true,
        actions: Vec::new(),
        inline_reply: None,
        desktop_entry: Some(kourier_desktop::APP_ID.to_owned()),
        category: Some(CATEGORY_SUMMARY.to_owned()),
        origin_name: origin.map(str::to_owned),
        image: None,
        urgency: Urgency::Normal,
        timeout_ms: -1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C1: ConversationId = ConversationId(1);
    const C2: ConversationId = ConversationId(2);
    const A1: AccountId = AccountId(1);
    const A2: AccountId = AccountId(2);
    /// The mode of a notification that shows the text.
    const TEXT: NotificationContent = NotificationContent::NameAndMessage;

    #[test]
    fn one_notification_per_conversation() {
        let mut b = NotificationBook::default();
        assert_eq!(b.replaces_for(C1), 0);
        b.shown_message(C1, A1, MessageId(10), 0, 100, TEXT);
        assert_eq!(b.replaces_for(C1), 100);
        b.shown_message(C1, A1, MessageId(11), 0, 100, TEXT);
        assert_eq!(b.count(C1), 2);
        assert_eq!(b.replaces_for(C2), 0, "other conversations get their own");
        b.shown_message(C2, A1, MessageId(20), 0, 101, TEXT);
        assert_eq!(b.messages(C1), vec![(MessageId(10), 0), (MessageId(11), 0)]);
        // The same message twice (a repeated Notify) is counted once.
        b.shown_message(C1, A1, MessageId(11), 0, 100, TEXT);
        assert_eq!(b.count(C1), 2);
        // The server closed it (dismissed): the next message starts a new one.
        b.on_closed(100, CloseReason::Dismissed);
        assert!(!b.is_tracked(C1));
        assert_eq!(b.replaces_for(C1), 0);
        b.shown_message(C1, A1, MessageId(12), 0, 102, TEXT);
        assert_eq!(b.count(C1), 1);
        assert_eq!(b.on_reply(100), None, "the old id means nothing now");
        assert_eq!(b.on_reply(102), Some(C1));
    }

    #[test]
    fn a_new_server_id_replaces_the_old_target() {
        let mut b = NotificationBook::default();
        b.shown_message(C1, A1, MessageId(10), 0, 100, TEXT);
        b.shown_message(C1, A1, MessageId(11), 0, 105, TEXT);
        assert_eq!(b.replaces_for(C1), 105);
        assert_eq!(b.on_reply(100), None);
        // A late close of the old id leaves the new one alone.
        b.on_closed(100, CloseReason::Dismissed);
        assert!(b.is_tracked(C1));
    }

    #[test]
    fn clicks_carry_the_token_and_the_target() {
        let mut b = NotificationBook::default();
        b.shown_message(C1, A1, MessageId(10), 0, 100, TEXT);
        b.on_token(100, "tok".into());
        b.on_token(999, "stray".into());
        let a = b.on_action(100, "default").expect("activation");
        assert_eq!(a.target, Target::Conversation(C1));
        assert_eq!(a.token.as_deref(), Some("tok"));
        // The token is used once.
        let again = b.on_action(100, "default").expect("activation");
        assert_eq!(again.token, None);
        // Other actions do not activate; unknown ids do nothing.
        assert!(b.on_action(100, "inline-reply").is_none());
        assert!(b.on_action(999, "default").is_none());
    }

    #[test]
    fn summaries_per_account() {
        let mut b = NotificationBook::default();
        assert_eq!(b.summary_replaces(A1), 0);
        b.shown_summary(A1, 7);
        b.shown_summary(A2, 8);
        assert_eq!(b.summary_replaces(A1), 7);
        b.shown_summary(A1, 9);
        assert_eq!(b.summary_replaces(A1), 9);
        let a = b.on_action(9, "default").expect("click");
        assert_eq!(a.target, Target::Summary(A1));
        assert_eq!(b.on_reply(9), None, "no replies to a summary");
        let mut closed = b.close_summaries();
        closed.sort_unstable();
        assert_eq!(closed, vec![8, 9]);
        assert_eq!(b.summary_replaces(A1), 0);
    }

    #[test]
    fn closing_by_conversation_and_account() {
        let mut b = NotificationBook::default();
        b.shown_message(C1, A1, MessageId(10), 0, 100, TEXT);
        b.shown_message(C2, A2, MessageId(20), 0, 101, TEXT);
        b.shown_summary(A2, 102);
        assert_eq!(b.close_conversation(C1), Some(100));
        assert_eq!(b.close_conversation(C1), None);
        let mut ids = b.close_account(A2);
        ids.sort_unstable();
        assert_eq!(ids, vec![101, 102]);
        assert!(b.conversations().is_empty());
        b.shown_message(C1, A1, MessageId(10), 0, 103, TEXT);
        assert_eq!(b.close_all(), vec![103]);
    }

    fn row(unread: u32, marked: bool) -> ConversationRow {
        ConversationRow {
            id: C1,
            account: A1,
            kind: kourier_model::ConvKind::Direct,
            is_self: false,
            title: None,
            peer: None,
            avatar: None,
            preview: None,
            last_activity: 0,
            unread,
            mentions: 0,
            marked_unread: marked,
            muted_until: None,
            local_muted_until: None,
            archived: false,
            pinned_order: None,
            blocked: false,
            request_pending: false,
            features_hash: 0,
            has_draft: false,
            hidden: false,
            can_send: true,
            roaming: kourier_core::facade::Roaming::Hidden,
            protected: false,
        }
    }

    fn notify(conv: i64, message: i64, mentions_me: bool) -> CoreNotification {
        CoreNotification {
            account: A1,
            conv: ConversationId(conv),
            message: MessageId(message),
            mentions_me,
        }
    }

    #[test]
    fn bursts_coalesce_per_conversation() {
        let bursts = coalesce(vec![
            notify(1, 10, false),
            notify(2, 20, false),
            notify(1, 11, true),
            notify(1, 11, true),
            notify(1, 12, false),
        ]);
        assert_eq!(bursts.len(), 2);
        assert_eq!(bursts[0].newest.message, MessageId(12));
        assert_eq!(bursts[0].earlier, vec![MessageId(10), MessageId(11)]);
        assert_eq!(bursts[1].newest.message, MessageId(20));
        assert!(bursts[1].earlier.is_empty());
        assert!(coalesce(Vec::new()).is_empty());
    }

    #[test]
    fn the_first_message_of_a_conversation_is_announced_at_once() {
        // The clock in milliseconds.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
        struct At(u64);
        impl std::ops::Add<std::time::Duration> for At {
            type Output = At;
            fn add(self, d: std::time::Duration) -> At {
                At(self.0 + u64::try_from(d.as_millis()).expect("short"))
            }
        }
        let rest = u64::try_from(COALESCE.as_millis()).expect("short");
        let (a, b) = (ConversationId(1), ConversationId(2));
        let mut pace = Pace::<At>::default();
        // A single message: nothing to wait for.
        assert_eq!(pace.admit(a, At(1000)), Admit::Now);
        pace.shown(a, At(1000));
        // A burst behind it is collected until the notification has rested; another
        // conversation's first message is not held back by it.
        assert_eq!(pace.admit(a, At(1010)), Admit::At(At(1000 + rest)));
        assert_eq!(pace.admit(a, At(1200)), Admit::At(At(1000 + rest)));
        assert_eq!(pace.admit(b, At(1200)), Admit::Now);
        pace.shown(b, At(1200));
        // The collected ones were shown when the rest was over: the conversation rests
        // again from there, so a long burst updates its notification once per rest.
        pace.shown(a, At(1000 + rest));
        assert_eq!(
            pace.admit(a, At(1000 + rest + 1)),
            Admit::At(At(1000 + 2 * rest))
        );
        // Quiet since then: the next message is the first of a new burst.
        assert_eq!(pace.admit(a, At(1000 + 2 * rest)), Admit::Now);
        assert_eq!(pace.admit(b, At(5000)), Admit::Now);
        // Nothing is remembered about conversations that are quiet.
        assert!(pace.resting.is_empty());
    }

    #[test]
    fn read_rules() {
        assert!(read_now(&row(0, false)));
        assert!(!read_now(&row(2, false)));
        assert!(!read_now(&row(0, true)), "marked unread keeps it");
    }

    #[test]
    fn a_reply_is_offered_only_where_the_chat_takes_a_message() {
        assert!(can_reply(&row(1, false)));
        let mut cannot = row(1, false);
        cannot.can_send = false;
        assert!(!can_reply(&cannot), "the core would refuse the send");
        let mut blocked = row(1, false);
        blocked.blocked = true;
        assert!(!can_reply(&blocked));
        let mut pending = row(1, false);
        pending.request_pending = true;
        assert!(!can_reply(&pending));
        // The notification carries it: no field to type into where it is not offered.
        let texts = Texts::default();
        let notification = |can_reply: bool| {
            message_notification(MessageContent {
                mode: NotificationContent::NameAndMessage,
                title: "Olena",
                line: "see you at 8",
                count_line: None,
                origin: None,
                avatar: None,
                can_reply,
                replaces_id: 0,
                texts: &texts,
            })
        };
        assert!(
            notification(can_reply(&row(1, false)))
                .inline_reply
                .is_some()
        );
        assert!(notification(can_reply(&cannot)).inline_reply.is_none());
    }

    fn msg(revision: u16, deleted: bool) -> MessageRow {
        MessageRow {
            id: MessageId(10),
            sort_key: 0,
            author: None,
            author_name: None,
            author_avatar: None,
            origin: kourier_core::facade::OriginKind::Remote,
            device: None,
            body: kourier_model::Body::Text(kourier_model::Text::plain("x")),
            reply: None,
            sent_at: 0,
            server_at: None,
            received_at: 0,
            revision,
            edited_at: None,
            deleted: deleted.then_some(kourier_model::Deletion::ForEveryone { by: None }),
            out_state: None,
            read_state: None,
            reactions: Vec::new(),
            expiry: None,
            view_once: false,
            history: false,
            mentions_me: false,
            outbox: None,
            actions: Default::default(),
        }
    }

    #[test]
    fn delete_and_edit_rules() {
        assert!(!changed_since_shown(0, Some(&msg(0, false))));
        assert!(changed_since_shown(0, Some(&msg(1, false))), "edited");
        assert!(changed_since_shown(0, Some(&msg(0, true))), "deleted");
        assert!(changed_since_shown(0, None), "gone (expired)");
    }

    /// A missed call that was announced and then turns out answered on the phone: its row
    /// is neither deleted nor edited, and the chat may hold other unread messages, so only
    /// the line's own read state says that the notification is no longer true.
    #[test]
    fn a_call_line_that_moved_on_is_withdrawn() {
        use kourier_model::{
            Body, CallDirection, CallLine, CallMedia, CallState, ReadState, SystemEvent,
        };
        let line = |state: CallState, read_state: Option<ReadState>| {
            let mut m = msg(0, false);
            m.body = Body::System(SystemEvent::Call(
                CallLine::signal(CallDirection::Incoming, false, state, 5)
                    .with_media(CallMedia::Voice, 5),
            ));
            m.read_state = read_state;
            m
        };
        let missed = line(CallState::Missed, Some(ReadState::Unread));
        assert!(!call_moved_on(&missed));
        assert!(!changed_since_shown(0, Some(&missed)), "still missed");
        // The core takes the read state of a line away when it stops counting.
        for state in [
            CallState::Answered,
            CallState::Declined,
            CallState::Ringing { deadline: 9 },
        ] {
            let moved = line(state, None);
            assert!(changed_since_shown(0, Some(&moved)), "{state:?}");
        }
        // Read here while other messages of the chat stay unread: no longer announced.
        let read = line(CallState::Missed, Some(ReadState::Read));
        assert!(changed_since_shown(0, Some(&read)));
        // An ordinary message has no read state to go by (the conversation's count does).
        let mut text = msg(0, false);
        text.read_state = Some(ReadState::Read);
        assert!(!call_moved_on(&text) && !changed_since_shown(0, Some(&text)));

        // The notification is closed only when it SHOWS the line that moved on; an older
        // line under a newer message is forgotten and the notification stays.
        let answered = line(CallState::Answered, None);
        assert_eq!(withdraw(0, Some(&answered), true), Withdraw::Close);
        assert_eq!(withdraw(0, Some(&answered), false), Withdraw::Forget);
        assert_eq!(withdraw(0, Some(&missed), true), Withdraw::Keep);
        assert_eq!(withdraw(0, Some(&missed), false), Withdraw::Keep);
        // Gone, deleted or edited closes it wherever the message stands.
        for newest in [true, false] {
            assert_eq!(withdraw(0, None, newest), Withdraw::Close);
            assert_eq!(withdraw(0, Some(&msg(0, true)), newest), Withdraw::Close);
            assert_eq!(withdraw(0, Some(&msg(1, false)), newest), Withdraw::Close);
            assert_eq!(withdraw(0, Some(&msg(0, false)), newest), Withdraw::Keep);
        }
        // The book: a line that is forgotten leaves the notification and its other messages.
        let mut book = NotificationBook::default();
        let conv = ConversationId(3);
        book.shown_message(conv, AccountId(1), MessageId(10), 0, 77, TEXT);
        book.shown_message(conv, AccountId(1), MessageId(11), 0, 77, TEXT);
        assert!(!book.forget_message(conv, MessageId(10)));
        assert_eq!(book.messages(conv), vec![(MessageId(11), 0)]);
        assert!(book.is_tracked(conv));
        assert!(book.forget_message(conv, MessageId(11)), "nothing is left");
        assert!(!book.forget_message(ConversationId(9), MessageId(1)));
    }

    fn content(mode: NotificationContent, texts: &Texts) -> Notification {
        message_notification(MessageContent {
            mode,
            title: "Olena",
            line: "see you at 8",
            count_line: Some("2 new messages"),
            origin: Some("Work"),
            avatar: ImageData::rgba(1, 1, vec![1, 2, 3, 4]),
            can_reply: true,
            replaces_id: 5,
            texts,
        })
    }

    /// A protected chat's notification shows no text of the message in any mode: the choice
    /// "name and message" becomes "name only" for it alone; the title, the count line, the
    /// account and the reply field (which shows no text) stay.
    #[test]
    fn a_protected_chat_s_notification_shows_no_text_in_any_mode() {
        let texts = Texts::default();
        let mut protected = row(1, false);
        protected.protected = true;
        let open = row(1, false);
        for chosen in [
            NotificationContent::NameAndMessage,
            NotificationContent::NameOnly,
            NotificationContent::Hidden,
        ] {
            let mode = content_mode(chosen, &protected);
            assert_ne!(mode, NotificationContent::NameAndMessage, "{chosen:?}");
            for count_line in [None, Some("2 new messages")] {
                let n = message_notification(MessageContent {
                    mode,
                    title: "Olena",
                    line: "the secret plan",
                    count_line,
                    origin: Some("Work"),
                    avatar: None,
                    can_reply: can_reply(&protected),
                    replaces_id: 0,
                    texts: &texts,
                });
                let reply = n.inline_reply.as_ref().map(|r| r.placeholder.as_str());
                for field in [
                    n.summary.as_str(),
                    n.body.as_str(),
                    n.app_name.as_str(),
                    n.origin_name.as_deref().unwrap_or(""),
                    reply.unwrap_or(""),
                ] {
                    assert!(!field.contains("secret"), "{chosen:?}: {field}");
                }
                if chosen != NotificationContent::Hidden {
                    assert_eq!(n.summary, "Olena", "the name stays");
                    assert_eq!(n.body, count_line.unwrap_or("New message"));
                    assert_eq!(reply, Some("Reply…"), "the reply field stays, without text");
                }
            }
        }
        // Not protected: the choice as it is.
        for chosen in [
            NotificationContent::NameAndMessage,
            NotificationContent::NameOnly,
            NotificationContent::Hidden,
        ] {
            assert_eq!(content_mode(chosen, &open), chosen);
        }
    }

    /// A chat that becomes protected after its text was shown: the book knows the mode each
    /// notification is shown in, the rule says which mode the one on screen is shown again in,
    /// and the re-show keeps (or renews) the id.
    #[test]
    fn a_notification_that_showed_text_is_shown_again_once_its_chat_is_protected() {
        let mut protected = row(1, false);
        protected.protected = true;
        let open = row(1, false);
        assert_eq!(
            text_to_withdraw(TEXT, &protected),
            Some(NotificationContent::NameOnly)
        );
        assert_eq!(
            text_to_withdraw(TEXT, &open),
            None,
            "not protected: as it is"
        );
        for shown in [NotificationContent::NameOnly, NotificationContent::Hidden] {
            assert_eq!(
                text_to_withdraw(shown, &protected),
                None,
                "{shown:?} has no text"
            );
        }
        let mut b = NotificationBook::default();
        assert_eq!(b.shown_in(C1), None);
        b.shown_message(C1, A1, MessageId(10), 0, 100, TEXT);
        assert_eq!(b.shown_in(C1), Some(TEXT));
        assert_eq!(b.shown_in(C2), None, "another chat has none");
        // Shown again without the text, in place.
        b.reshown(C1, 100, NotificationContent::NameOnly);
        assert_eq!(b.shown_in(C1), Some(NotificationContent::NameOnly));
        assert_eq!((b.replaces_for(C1), b.count(C1)), (100, 1));
        // The server gave a new id (the old one was gone meanwhile): the book follows it.
        b.reshown(C1, 101, NotificationContent::NameOnly);
        assert_eq!(b.replaces_for(C1), 101);
        b.on_closed(100, CloseReason::Expired);
        assert!(b.is_tracked(C1), "the old id means nothing any more");
        // A later message in text mode (the chat was lifted again) is remembered as such.
        b.shown_message(C1, A1, MessageId(11), 0, 101, TEXT);
        assert_eq!(b.shown_in(C1), Some(TEXT));
        // A chat the book does not track is not shown again.
        b.reshown(C2, 200, NotificationContent::NameOnly);
        assert!(!b.is_tracked(C2) && b.on_reply(200).is_none());
    }

    /// The server's history keeps what EXPIRED (Plasma drops a notification the user
    /// dismissed or the client closed): only an expired notification that showed text is
    /// remembered, per chat, for revoking when the chat becomes protected; the memory is
    /// bounded and ends with the server.
    #[test]
    fn only_an_expired_notification_that_showed_text_is_remembered_for_revoking() {
        use CloseReason::{Closed, Dismissed, Expired, Other};
        use NotificationContent::{Hidden, NameOnly};
        for (mode, reason, remembered) in [
            (TEXT, Expired, true),
            (TEXT, Dismissed, false),
            (TEXT, Closed, false),
            (TEXT, Other, false),
            (NameOnly, Expired, false),
            (Hidden, Expired, false),
        ] {
            let mut b = NotificationBook::default();
            b.shown_message(C1, A1, MessageId(10), 0, 100, mode);
            b.shown_message(C2, A1, MessageId(20), 0, 101, TEXT);
            b.on_closed(100, reason);
            assert!(!b.is_tracked(C1), "{mode:?} {reason:?}");
            assert_eq!(b.has_expired_text(C1), remembered, "{mode:?} {reason:?}");
            assert!(!b.has_expired_text(C2), "another chat's is not touched");
            let expected: Vec<u32> = if remembered { vec![100] } else { Vec::new() };
            assert_eq!(b.take_expired_text(C1), expected, "{mode:?} {reason:?}");
            assert!(!b.has_expired_text(C1), "taken once");
        }
        // An id the book replaced meanwhile (a new id after a re-show) is not remembered.
        let mut b = NotificationBook::default();
        b.shown_message(C1, A1, MessageId(10), 0, 100, TEXT);
        b.shown_message(C1, A1, MessageId(11), 0, 105, TEXT);
        b.on_closed(100, CloseReason::Expired);
        assert!(
            !b.has_expired_text(C1),
            "100 is no longer the chat's notification"
        );
        // Several ids of one chat come out together, oldest first; a gone chat forgets them.
        let mut b = NotificationBook::default();
        for (m, id) in [(1, 300u32), (2, 301), (3, 302)] {
            b.shown_message(C1, A1, MessageId(m), 0, id, TEXT);
            b.on_closed(id, CloseReason::Expired);
        }
        assert_eq!(b.expired_chats(), vec![C1], "each chat once");
        assert_eq!(b.take_expired_text(C1), vec![300, 301, 302]);
        assert!(b.expired_chats().is_empty());
        b.shown_message(C2, A1, MessageId(9), 0, 400, TEXT);
        b.on_closed(400, CloseReason::Expired);
        assert_eq!(b.expired_chats(), vec![C2]);
        b.forget_expired_text(C2);
        assert!(!b.has_expired_text(C2));
        assert!(b.expired_chats().is_empty());
        // Bounded: the oldest ids go first.
        let mut b = NotificationBook::default();
        for id in 1..=(MAX_EXPIRED_TEXT as u32 + 3) {
            b.shown_message(C1, A1, MessageId(i64::from(id)), 0, id, TEXT);
            b.on_closed(id, CloseReason::Expired);
        }
        let kept = b.take_expired_text(C1);
        assert_eq!(kept.len(), MAX_EXPIRED_TEXT);
        assert_eq!(kept.first(), Some(&4), "the first three are gone");
        // The server changed (or Kourier quits): everything is forgotten.
        b.shown_message(C1, A1, MessageId(1), 0, 900, TEXT);
        b.on_closed(900, CloseReason::Expired);
        b.close_all();
        assert!(!b.has_expired_text(C1));
    }

    #[test]
    fn content_modes() {
        let texts = Texts::default();
        let full = content(NotificationContent::NameAndMessage, &texts);
        assert_eq!(full.summary, "Olena");
        assert_eq!(full.body, "see you at 8");
        assert!(full.image.is_some() && full.inline_reply.is_some());
        assert_eq!(full.origin_name.as_deref(), Some("Work"));
        assert_eq!(full.replaces_id, 5);
        assert_eq!(full.category.as_deref(), Some(CATEGORY_MESSAGE));
        assert_eq!(full.desktop_entry.as_deref(), Some(kourier_desktop::APP_ID));
        assert!(full.default_action);

        let name = content(NotificationContent::NameOnly, &texts);
        assert_eq!(name.summary, "Olena");
        assert_eq!(name.body, "2 new messages");
        assert!(!name.body.contains("see you"));

        let hidden = content(NotificationContent::Hidden, &texts);
        assert_eq!(hidden.summary, "Kourier");
        assert_eq!(hidden.body, "New message");
        assert!(hidden.image.is_none());
        assert!(hidden.origin_name.is_none());
        assert!(hidden.inline_reply.is_none());
        assert!(hidden.default_action, "a click still opens Kourier");
    }

    #[test]
    fn summary_content() {
        let texts = Texts::default();
        let n = summary_notification("12 new messages in 4 chats", Some("Work"), 3, &texts);
        assert_eq!(n.summary, "Kourier");
        assert_eq!(n.body, "12 new messages in 4 chats");
        assert_eq!(n.replaces_id, 3);
        assert!(n.inline_reply.is_none());
        assert_eq!(n.origin_name.as_deref(), Some("Work"));
    }
}
