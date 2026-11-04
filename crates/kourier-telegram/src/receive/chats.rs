//! The chat machine (TG-3 C5a; brief §3.1-§3.2, critic H1, H5, L4, L6, L-13, decisions Q-L9).
//!
//! - LEARN, then STAGE (H1): TDLib tells a chat (`updateNewChat`) and a user (`updateUser`) ONCE per
//!   instance, and on a fresh link it does so between `Ready` and the core's ack of `AccountReady`.
//!   The machine keeps chats, users, supergroups and basic groups in memory from the first update
//!   on, and stages nothing before [`Chats::link`]; the link stages everything learnt so far.
//!   Before the link the memory is bounded ([`LEARN_BOUND`] objects of the four kinds together,
//!   Q-L9); a chat told past the bound is asked again with `getChat` at the link (a [`Fetch`]), a
//!   user past it is lost for this helper run (no request T3 sends gets it back: its private chat
//!   stays undecided, so it is not listed).
//! - THE LISTING RULE (brief §3.2, L-13): a chat is a CONVERSATION when it has a position with
//!   `order != 0` in the main list or the archive, when TDLib added it to one of those lists
//!   (`updateChatAddedToList`: with the message DB off the fixture's chat comes that way, its
//!   `positions` empty), or when a message of it was delivered ([`Chats::on_delivered`]). Every other
//!   chat TDLib tells (a forward's origin, a linked channel) stays in `chat_state` only (`listed = 0`).
//!   Leaving the lists removes nothing (no `ConversationRemoved` in T3).
//! - HOLDS until T7 (his TG4, H5, L4): broadcast channels, bot chats (`userTypeBot`), public
//!   supergroups (an active username) and channels' direct-messages groups: kept in `chat_state`
//!   with `held`, never a conversation. A chat whose supergroup or user TDLib has not told yet is
//!   UNDECIDED and not listed (TDLib tells those objects before the chat; this is the belt).
//!   Secret chats are never mapped (none on another device).
//! - A chat named by a fact or a message that the machine never learnt is asked once with
//!   `getChat` ([`Chats::reference`]); before the link the ask waits for the link.
//! - The basic-group upgrade (RC11): `updateBasicGroup.upgraded_to_supergroup_id` records the
//!   supergroup's chat id as the old chat's `moved_to` (a core "moved conversation" shape is
//!   later); both chats stay as they are.
//! - The hash gate (critic L6): a chat's patch is gated by its stated hash, except with the chat's
//!   first message row of a helper run ([`Chats::row_upsert`]), which restates it.
//! - FACTS AT ONCE, PATCHES AT A FLUSH (C5b): a chat's facts are staged with the update that
//!   changed them; its conversation patch (and a needed user's contact patch, and the pinned list)
//!   only at [`Chats::flush`], which the actor calls once per commit batch (C7b). TDLib re-tells a
//!   chat in several updates (a restart's `updateNewChat`, its read state 0, then the real one):
//!   only the state at the flush is stated, so the gate leaves an unchanged chat out.
//! - THE PATCH (C5b, brief §3.2): title (`plain_line`), `archived` from the archive list (folders
//!   are T8), mute from the chat's or its scope's `mute_for`, `marked_unread`, the timer, the unread
//!   hint (unread messages WITHOUT rows, critic M4), `meta` (`tg-chat` v1: protected, restricted)
//!   and the features ([`features`]). The pinned list is `PinnedConversations`, main-list pins by
//!   order, highest first (archive pins are not Kourier pins).
//! - USERS (brief §3.5, L3, L-8): a contact patch, hash-gated per user, only for the users Kourier
//!   needs: a listed private chat's peer, a contact (`is_contact`), a delivered message's author
//!   ([`Chats::author_upsert`]); a chat author (`messageSenderChat`) is `tg:<signed chat id>` with
//!   the chat's title ([`Chats::sender_chat_upsert`]). The own account states nothing.
//!
//! Nothing here logs a value; `Debug` prints ids and counts.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_model::{
    BackendBlob, ContactPatch, ConvKey, ConversationFeatures, ConversationPatch, HistorySource,
    Identifier, IdentifierKind, Patch, UserId, Window,
};

use super::{Fetch, Out};
use crate::login::plain_line;
use crate::store::{ChatFacts, ChatKind, Event, Gate, Held, Row};
use crate::tdapi::{
    BasicGroup, Chat, ChatList, ChatPosition, ChatType, NotificationScope, Receive, Supergroup,
    TgUser, UserType,
};

/// A `mute_for` above this many seconds is "until unmuted" (`i64::MAX`): Telegram's "forever" is
/// a date years ahead, TDLib hands the seconds left (brief §3.2).
const MUTE_FOREVER_SECS: i64 = 366 * 24 * 60 * 60;

/// The version of the `tg-chat` and `tg-user` meta blobs.
const META_VERSION: u16 = 1;

/// How many chats, users, supergroups and basic groups the machine learns before the link
/// (decision Q-L9).
pub(crate) const LEARN_BOUND: usize = 4096;

/// How often `getChat` is asked for one chat in a helper run (fix round TF3: a failed ask is
/// forgotten so the chat's next message asks once more).
pub(crate) const CHAT_TRIES: u8 = 3;

/// TDLib's chat id of a supergroup (or channel) is this minus its id (`ZERO_CHANNEL_ID`).
const ZERO_CHANNEL_ID: i64 = -1_000_000_000_000;

/// The lists Kourier maps: the main list and the archive (folders are T8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Listed {
    Main,
    Archive,
}

impl Listed {
    fn of(list: ChatList) -> Option<Self> {
        match list {
            ChatList::Main => Some(Self::Main),
            ChatList::Archive => Some(Self::Archive),
            ChatList::Folder { .. } | ChatList::Unknown => None,
        }
    }
}

/// What the machine keeps of one chat.
#[derive(Debug)]
struct Known {
    chat: Chat,
    /// The lists TDLib added it to (`updateChatAddedToList`), until removed.
    added: BTreeSet<Listed>,
    /// A message of it was delivered (it is a conversation from then on).
    has_rows: bool,
    /// A basic group's supergroup chat (RC11).
    moved_to: Option<i64>,
}

impl Known {
    fn new(chat: Chat) -> Self {
        Self {
            chat,
            added: BTreeSet::new(),
            has_rows: false,
            moved_to: None,
        }
    }

    fn set_position(&mut self, pos: ChatPosition) {
        self.chat.positions.retain(|p| p.list != pos.list);
        if pos.order != 0 {
            self.chat.positions.push(pos);
        } else if let Some(list) = Listed::of(pos.list) {
            self.added.remove(&list);
        }
    }

    fn in_list(&self) -> bool {
        self.in_one(Listed::Main) || self.in_one(Listed::Archive)
    }

    /// In `list`: a position with `order != 0` there, or added to it.
    fn in_one(&self, list: Listed) -> bool {
        self.added.contains(&list)
            || self
                .chat
                .positions
                .iter()
                .any(|p| p.order != 0 && Listed::of(p.list) == Some(list))
    }

    /// The main-list pin's order, when the chat is pinned there.
    fn main_pin(&self) -> Option<i64> {
        self.chat
            .positions
            .iter()
            .find(|p| p.list == ChatList::Main && p.is_pinned && p.order != 0)
            .map(|p| p.order)
    }
}

/// The scope whose defaults a chat of this type takes (`td_api.tl:3647-3653`).
fn scope_of(chat: &Chat) -> NotificationScope {
    match chat.r#type {
        ChatType::Private { .. } | ChatType::Secret { .. } => NotificationScope::Private,
        ChatType::Supergroup {
            is_channel: true, ..
        } => NotificationScope::Channel,
        ChatType::BasicGroup { .. } | ChatType::Supergroup { .. } => NotificationScope::Group,
        ChatType::Unknown => NotificationScope::Unknown,
    }
}

/// The features of a Telegram conversation in T3 (critic M1, refresh §2 C5b): edits are taken
/// without a window (the server validates them; Telegram edits older messages, Saved Messages
/// among them), received deletes have no window yet (they are parked until KT-2, G-8), read
/// receipts and typing are always on (his TG6). The send side (formatting, files, the edit and
/// delete windows) is T5's.
pub(crate) fn features() -> ConversationFeatures {
    ConversationFeatures {
        edit_receive: Some(Window {
            max_age_secs: None,
            max_count: None,
        }),
        delete_receive: None,
        read_receipts: true,
        typing: true,
        history: HistorySource::OnDemand,
        ..ConversationFeatures::default()
    }
}

/// A user's contact patch (brief §3.5, L3): the name through `plain_line` (cleared for a deleted
/// account), the active usernames and the number Telegram shows, with the number verdict.
fn contact_patch(user: &TgUser, id: &UserId) -> ContactPatch {
    let mut patch = ContactPatch::new(id.clone());
    let name = format!("{} {}", user.first_name.as_str(), user.last_name.as_str());
    patch.profile_name = match plain_line(name.trim()) {
        Some(name) if user.r#type != UserType::Deleted => Patch::Set(name),
        _ => Patch::Clear,
    };
    let mut identifiers: Vec<Identifier> = user
        .usernames
        .iter()
        .flat_map(|u| &u.active_usernames)
        .filter(|n| !n.is_empty())
        .map(|n| Identifier {
            kind: IdentifierKind::Username,
            value: n.as_str().to_owned(),
        })
        .collect();
    let shown = !user.phone_number.is_empty();
    if shown {
        identifiers.push(Identifier {
            kind: IdentifierKind::Phone,
            value: format!("+{}", user.phone_number.as_str()),
        });
    }
    patch.identifiers = Some(identifiers);
    patch.number_shown = Some(shown);
    let restricted = user
        .restriction_info
        .as_ref()
        .is_some_and(|r| !r.restriction_reason.is_empty());
    patch.meta = Patch::Set(meta(&serde_json::json!({
        "bot": user.r#type == UserType::Bot,
        "deleted": user.r#type == UserType::Deleted,
        "restricted": restricted,
    })));
    patch
}

/// A `tg-chat`/`tg-user` meta blob (opaque to the core).
fn meta(value: &serde_json::Value) -> BackendBlob {
    BackendBlob::new(META_VERSION, value.to_string().into_bytes())
}

/// `tg:<id>`: a user's id, or a chat author's signed chat id (L-8; L2: a leading `-` passes the
/// scoped-id rule).
fn tg_user(id: i64) -> Option<UserId> {
    UserId::from_parts("tg", &id.to_string()).ok()
}

/// The chat machine of one helper run (see the module doc).
#[derive(Debug, Default)]
pub(crate) struct Chats {
    /// The account's own TDLib user id (saved messages are the chat with it).
    me: Option<i64>,
    linked: bool,
    chats: HashMap<i64, Known>,
    users: HashMap<i64, TgUser>,
    supergroups: HashMap<i64, Supergroup>,
    basic_groups: HashMap<i64, BasicGroup>,
    /// Chats to ask with `getChat` at the link (told past the bound, or referenced before it).
    missing: BTreeSet<i64>,
    /// `getChat` asked and not answered yet.
    asked: HashSet<i64>,
    /// How often `getChat` was asked per chat in this helper run (at most [`CHAT_TRIES`]).
    tries: HashMap<i64, u8>,
    /// `getUser` asked in this run (Q-B1: once per id per helper run, answered or not).
    asked_users: HashSet<i64>,
    /// `getSupergroup` asked in this run (TF4: once per id per helper run, answered or not).
    asked_supergroups: HashSet<i64>,
    /// `getBasicGroup` asked in this run (TF4: once per id per helper run).
    asked_basic_groups: HashSet<i64>,
    /// The last facts staged per chat (a fact is staged only when it changed).
    facts: HashMap<i64, ChatFacts>,
    /// Chats whose patch went out with a message row in this run (critic L6).
    restated: HashSet<i64>,
    /// Objects dropped by the bound before the link.
    dropped: usize,
    /// The bound a test lowered (TG-4 K6b); `None` is the product's [`LEARN_BOUND`]. A test
    /// build only (U9: the kill child is the test binary).
    #[cfg(test)]
    learn_bound: Option<usize>,
    /// The clock of the mute (ms), set by the actor before each call ([`Chats::set_now`]).
    now_ms: i64,
    /// The scopes' default `mute_for` (`updateScopeNotificationSettings`; 0 until told).
    scope_mute: BTreeMap<ScopeKey, i32>,
    /// Per chat, the incoming delivered message ids above its read position (critic M4: the
    /// unread hint leaves them out). Seeded from `delivered` at the start ([`Chats::seed_unread`]).
    unread_rows: HashMap<i64, BTreeSet<i64>>,
    /// Users a delivered message named as its author (they are needed from then on).
    authors: HashSet<i64>,
    /// Chats whose patch waits for the next flush.
    dirty: BTreeSet<i64>,
    /// Users whose contact patch waits for the next flush.
    dirty_users: BTreeSet<i64>,
    /// The pinned list stated last in this run (`None`: none yet; the first flush states it).
    pins: Option<Vec<ConvKey>>,
}

/// A [`NotificationScope`] as a map key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ScopeKey {
    Private,
    Group,
    Channel,
}

impl ScopeKey {
    fn of(scope: NotificationScope) -> Option<Self> {
        match scope {
            NotificationScope::Private => Some(Self::Private),
            NotificationScope::Group => Some(Self::Group),
            NotificationScope::Channel => Some(Self::Channel),
            NotificationScope::Unknown => None,
        }
    }
}

impl Chats {
    /// A machine for a helper run; `me` is the account's user id when it is known (a resume).
    pub(crate) fn new(me: Option<i64>) -> Self {
        Self {
            me,
            ..Self::default()
        }
    }

    /// Lowers the bound before the link to `bound` objects (TG-4 K6b: the supervisor's
    /// `Timings::learn_bound`, so a kill test reaches the waiting path with a handful of objects;
    /// the product never sets it: it exists in a test build only, U9).
    #[cfg(test)]
    pub(crate) fn set_learn_bound(&mut self, bound: usize) {
        self.learn_bound = Some(bound);
    }

    /// How many objects the machine learns before the link.
    fn bound(&self) -> usize {
        #[cfg(test)]
        if let Some(bound) = self.learn_bound {
            return bound;
        }
        LEARN_BOUND
    }

    /// The link (a resume's start, or the core's ack of a login's `AccountReady`): from now on
    /// everything learnt is staged; the first [`Out`] states every chat learnt so far and asks
    /// the chats the bound dropped.
    pub(crate) fn link(&mut self, me: i64) -> Out {
        self.me = Some(me);
        self.linked = true;
        let mut out = Out::default();
        let mut ids: Vec<i64> = self.chats.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            self.stage(id, &mut out);
        }
        for id in std::mem::take(&mut self.missing) {
            self.ask(id, &mut out);
        }
        let contacts: Vec<i64> = self
            .users
            .values()
            .filter(|u| u.is_contact)
            .map(|u| u.id)
            .collect();
        self.dirty_users.extend(contacts);
        out.extend(self.flush());
        out
    }

    /// The clock the mute is counted from (ms since the epoch); the actor sets it before a call.
    pub(crate) fn set_now(&mut self, now_ms: i64) {
        self.now_ms = now_ms;
    }

    /// The patches waiting since the last flush (see the module doc): the needed users' contact
    /// patches first, then each changed chat's conversation patch (gated), then the pinned list
    /// when it changed. Nothing before the link.
    pub(crate) fn flush(&mut self) -> Out {
        let mut out = Out::default();
        if !self.linked {
            return out;
        }
        let chats = std::mem::take(&mut self.dirty);
        let mut users = std::mem::take(&mut self.dirty_users);
        let mut patches = Vec::new();
        for id in &chats {
            if let Some(patch) = self.listed_patch(*id) {
                if let Some(ChatType::Private { user_id }) =
                    self.chats.get(id).map(|k| k.chat.r#type)
                {
                    users.insert(user_id);
                }
                patches.push((*id, patch));
            }
        }
        for user_id in users {
            if self.needs_user(user_id)
                && let Some(event) = self.user_event(user_id)
            {
                out.staged.rows.push(Row {
                    events: vec![event],
                    delivered: Vec::new(),
                });
            }
        }
        for (chat_id, patch) in patches {
            out.staged.rows.push(Row {
                events: vec![Event {
                    event: BackendEvent::ConversationUpsert(patch),
                    gate: Some(Gate::Chat(chat_id)),
                }],
                delivered: Vec::new(),
            });
        }
        if !chats.is_empty() || self.pins.is_none() {
            let pins = self.pinned();
            if self.pins.as_ref() != Some(&pins) {
                self.pins = Some(pins.clone());
                out.staged.rows.push(Row {
                    events: vec![Event {
                        event: BackendEvent::PinnedConversations(pins),
                        gate: None,
                    }],
                    delivered: Vec::new(),
                });
            }
        }
        out
    }

    /// Seeds the chat's unread rows from `delivered` at a helper run's start (C7a: the incoming
    /// delivered ids above `read_inbox_max_id`; the sweep keeps every one of them, critic M4).
    pub(crate) fn seed_unread(&mut self, chat_id: i64, ids: impl IntoIterator<Item = i64>) {
        self.unread_rows.entry(chat_id).or_default().extend(ids);
        self.dirty.insert(chat_id);
    }

    /// A delivered message's author (C6a): its contact patch (gated) to put before the message,
    /// `None` for the own account or a user the machine does not know.
    pub(crate) fn author_upsert(&mut self, user_id: i64) -> Option<Event> {
        self.authors.insert(user_id);
        self.dirty_users.remove(&user_id);
        self.user_event(user_id)
    }

    /// A user a delivered message named as its author: asked with `getUser` when the machine does
    /// not know it (Q-B1; its contact patch then follows at the next flush, `authors` holds it).
    pub(crate) fn reference_user(&mut self, user_id: i64) -> Out {
        let mut out = Out::default();
        self.ask_user(user_id, &mut out);
        out
    }

    /// A chat as a message's author (`messageSenderChat`: a channel posting in a group, an
    /// anonymous admin; L-8): the contact `tg:<signed chat id>` with the chat's title, gated by
    /// that id. `None` for a chat the machine does not know.
    pub(crate) fn sender_chat_upsert(&self, chat_id: i64) -> Option<Event> {
        let k = self.chats.get(&chat_id)?;
        let mut patch = ContactPatch::new(tg_user(chat_id)?);
        patch.profile_name = plain_line(k.chat.title.as_str()).map_or(Patch::Clear, Patch::Set);
        Some(Event {
            event: BackendEvent::ContactUpsert(patch),
            gate: Some(Gate::User(chat_id)),
        })
    }

    /// Staging is open.
    pub(crate) fn is_linked(&self) -> bool {
        self.linked
    }

    /// The account's own user id (known from the link on).
    pub(crate) fn me(&self) -> Option<i64> {
        self.me
    }

    /// The own account's active usernames as TDLib told them (C6a: `@<name>` mentions us).
    pub(crate) fn my_usernames(&self) -> Vec<String> {
        self.me
            .and_then(|me| self.users.get(&me))
            .and_then(|u| u.usernames.as_ref())
            .map(|u| {
                u.active_usernames
                    .iter()
                    .filter(|n| !n.is_empty())
                    .map(|n| n.as_str().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Objects the bound dropped before the link (tests only).
    #[cfg(test)]
    pub(crate) fn dropped(&self) -> usize {
        self.dropped
    }

    /// One receive update. Message updates are not the chat machine's (C6a/C6b) and give nothing.
    pub(crate) fn on_update(&mut self, update: &Receive) -> Out {
        let mut out = Out::default();
        match update {
            Receive::NewChat(chat) => self.learn_chat((**chat).clone(), &mut out),
            Receive::User(user) => self.learn_user((**user).clone(), &mut out),
            Receive::Supergroup(sg) => self.learn_supergroup((**sg).clone(), &mut out),
            Receive::BasicGroup(bg) => self.learn_basic_group(*bg, &mut out),
            Receive::ChatTitle(u) => {
                self.with_chat(u.chat_id, &mut out, |k| k.chat.title = u.title.clone())
            }
            Receive::ChatPhoto(u) => {
                self.with_chat(u.chat_id, &mut out, |k| k.chat.photo = u.photo.clone())
            }
            Receive::ChatPosition(u) => {
                self.with_chat(u.chat_id, &mut out, |k| k.set_position(u.position))
            }
            Receive::ChatLastMessage(u) => self.with_chat(u.chat_id, &mut out, |k| {
                k.chat.last_message.clone_from(&u.last_message);
                for p in &u.positions {
                    k.set_position(*p);
                }
            }),
            Receive::ChatAddedToList(u) => self.with_chat(u.chat_id, &mut out, |k| {
                if let Some(list) = Listed::of(u.chat_list) {
                    k.added.insert(list);
                }
            }),
            Receive::ChatRemovedFromList(u) => self.with_chat(u.chat_id, &mut out, |k| {
                if let Some(list) = Listed::of(u.chat_list) {
                    k.added.remove(&list);
                    k.chat
                        .positions
                        .retain(|p| Listed::of(p.list) != Some(list));
                }
            }),
            Receive::ChatReadInbox(u) => {
                if let Some(rows) = self.unread_rows.get_mut(&u.chat_id) {
                    rows.retain(|id| *id > u.last_read_inbox_message_id);
                }
                self.with_chat(u.chat_id, &mut out, |k| {
                    k.chat.last_read_inbox_message_id = u.last_read_inbox_message_id;
                    k.chat.unread_count = u.unread_count;
                });
            }
            Receive::ChatReadOutbox(u) => self.with_chat(u.chat_id, &mut out, |k| {
                k.chat.last_read_outbox_message_id = u.last_read_outbox_message_id;
            }),
            Receive::ChatNotificationSettings(u) => self.with_chat(u.chat_id, &mut out, |k| {
                k.chat.notification_settings = u.notification_settings;
            }),
            Receive::ChatIsMarkedAsUnread(u) => self.with_chat(u.chat_id, &mut out, |k| {
                k.chat.is_marked_as_unread = u.is_marked_as_unread;
            }),
            Receive::ChatMessageAutoDeleteTime(u) => self.with_chat(u.chat_id, &mut out, |k| {
                k.chat.message_auto_delete_time = u.message_auto_delete_time;
            }),
            Receive::ChatHasProtectedContent(u) => self.with_chat(u.chat_id, &mut out, |k| {
                k.chat.has_protected_content = u.has_protected_content;
            }),
            Receive::ScopeNotificationSettings(u) => {
                if let Some(scope) = ScopeKey::of(u.scope) {
                    self.scope_mute
                        .insert(scope, u.notification_settings.mute_for);
                    // Every chat that takes this scope's default changes with it.
                    let ids: Vec<i64> = self
                        .chats
                        .values()
                        .filter(|k| {
                            k.chat.notification_settings.use_default_mute_for
                                && ScopeKey::of(scope_of(&k.chat)) == Some(scope)
                        })
                        .map(|k| k.chat.id)
                        .collect();
                    for id in ids {
                        self.stage(id, &mut out);
                    }
                }
            }
            Receive::ChatAction(_)
            | Receive::NewMessage(_)
            | Receive::MessageContent(_)
            | Receive::MessageEdited(_)
            | Receive::DeleteMessages(_) => {}
        }
        out
    }

    /// The answer of a [`Fetch::Chat`]: learnt like an `updateNewChat`.
    pub(crate) fn on_chat(&mut self, chat: Chat) -> Out {
        let mut out = Out::default();
        self.asked.remove(&chat.id);
        self.learn_chat(chat, &mut out);
        out
    }

    /// A [`Fetch::Chat`] failed (TDLib does not know the chat, or the link failed): the id is
    /// forgotten, so the chat's next message asks once more (TF3), up to [`CHAT_TRIES`] asks per
    /// chat in a helper run.
    pub(crate) fn on_chat_failed(&mut self, chat_id: i64) {
        self.asked.remove(&chat_id);
    }

    /// The answer of a [`Fetch::Supergroup`] (TF4): learnt like an `updateSupergroup` (after the
    /// link the bound does not apply), so a supergroup chat that waited for it is decided now.
    pub(crate) fn on_supergroup(&mut self, sg: Supergroup) -> Out {
        let mut out = Out::default();
        self.learn_supergroup(sg, &mut out);
        out
    }

    /// The answer of a [`Fetch::BasicGroup`] (TF4): learnt like an `updateBasicGroup`.
    pub(crate) fn on_basic_group(&mut self, bg: BasicGroup) -> Out {
        let mut out = Out::default();
        self.learn_basic_group(bg, &mut out);
        out
    }

    /// The chat is known but not decided yet: it waits for a user or a supergroup object that
    /// was not told (TF4). A secret chat or one of an unknown type is never decided: not this.
    pub(crate) fn deciding(&self, chat_id: i64) -> bool {
        self.chats.get(&chat_id).is_some_and(|k| {
            matches!(
                k.chat.r#type,
                ChatType::Private { .. } | ChatType::Supergroup { .. }
            ) && self.hold(&k.chat).is_err()
        })
    }

    /// Every known chat that is not decided yet ([`Chats::deciding`]).
    pub(crate) fn deciding_ids(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .chats
            .keys()
            .copied()
            .filter(|id| self.deciding(*id))
            .collect();
        ids.sort_unstable();
        ids
    }

    /// The chat is mapped and in a list (or has rows): the fresh-link load's kind of chat.
    pub(crate) fn is_listed_mapped(&self, chat_id: i64) -> bool {
        self.chats
            .get(&chat_id)
            .is_some_and(|k| (k.in_list() || k.has_rows) && self.is_mapped(chat_id))
    }

    /// The answer of a [`Fetch::User`] (Q-B1): learnt like an `updateUser` (after the link the
    /// bound does not apply), so a private chat that waited for its user is decided now.
    pub(crate) fn on_user(&mut self, user: TgUser) -> Out {
        let mut out = Out::default();
        self.learn_user(user, &mut out);
        out
    }

    /// A [`Fetch::User`] failed: not asked again in this run (the chat stays undecided until the
    /// next helper run).
    pub(crate) fn on_user_failed(&mut self, _user_id: i64) {
        // `asked_users` keeps the id.
    }

    /// A message or a fact names `chat_id`: an unknown chat is asked once with `getChat` (after the
    /// link; before it the ask waits for the link).
    pub(crate) fn reference(&mut self, chat_id: i64) -> Out {
        let mut out = Out::default();
        if chat_id != 0 && !self.chats.contains_key(&chat_id) {
            if self.linked {
                self.ask(chat_id, &mut out);
            } else if self.missing.len() < self.bound() {
                self.missing.insert(chat_id);
            }
        }
        out
    }

    /// A message of `chat_id` is being delivered (C6a): the chat is a conversation from now on,
    /// and an incoming message above the read position is an unread ROW (the hint leaves it out,
    /// critic M4). Call it BEFORE [`Chats::row_upsert`] of the same row, so the patch in that row
    /// carries the hint that counts it.
    pub(crate) fn on_delivered(&mut self, chat_id: i64, msg_id: i64, outgoing: bool) -> Out {
        let mut out = Out::default();
        let Some(k) = self.chats.get_mut(&chat_id) else {
            return out;
        };
        let unread = !outgoing && msg_id > k.chat.last_read_inbox_message_id;
        let first = !std::mem::replace(&mut k.has_rows, true);
        if unread {
            self.unread_rows.entry(chat_id).or_default().insert(msg_id);
            self.dirty.insert(chat_id);
        }
        if first {
            self.stage(chat_id, &mut out);
        }
        out
    }

    /// The chat is known and mapped (not held, not undecided, not secret): its messages may become
    /// rows.
    pub(crate) fn is_mapped(&self, chat_id: i64) -> bool {
        self.chats
            .get(&chat_id)
            .is_some_and(|k| matches!(self.hold(&k.chat), Ok(None)) && key(&k.chat).is_some())
    }

    /// Why the chat is held; `None` for a mapped, an undecided or an unknown chat (tests only).
    #[cfg(test)]
    pub(crate) fn held(&self, chat_id: i64) -> Option<Held> {
        self.chats
            .get(&chat_id)
            .and_then(|k| self.hold(&k.chat).ok().flatten())
    }

    /// The chat is held NOW (a channel, a bot, a public supergroup, direct messages; H5), whatever
    /// it was when its rows came (TF11): it gets no read act and no reconcile page.
    pub(crate) fn is_held(&self, chat_id: i64) -> bool {
        self.chats
            .get(&chat_id)
            .is_some_and(|k| matches!(self.hold(&k.chat), Ok(Some(_))))
    }

    /// The machine learnt this chat.
    pub(crate) fn knows(&self, chat_id: i64) -> bool {
        self.chats.contains_key(&chat_id)
    }

    /// What TDLib told of the chat (its last message, unread count and read positions).
    pub(crate) fn chat(&self, chat_id: i64) -> Option<&Chat> {
        self.chats.get(&chat_id).map(|k| &k.chat)
    }

    /// The listed, mapped chats in the start sync's order (brief §4.5: the main list's top
    /// first, then the archive's, then the rest by id). Held, undecided and secret chats are not
    /// in it (Q-B3: never fetched).
    pub(crate) fn listed_in_order(&self) -> Vec<i64> {
        let order = |k: &Known, list: ChatList| {
            k.chat
                .positions
                .iter()
                .find(|p| p.list == list && p.order != 0)
                .map_or(0, |p| p.order)
        };
        let mut ids: Vec<(u8, i64, i64)> = self
            .chats
            .values()
            .filter(|k| (k.in_list() || k.has_rows) && self.is_mapped(k.chat.id))
            .map(|k| {
                let main = order(k, ChatList::Main);
                let archive = order(k, ChatList::Archive);
                let rank = if k.in_one(Listed::Main) {
                    0
                } else if k.in_one(Listed::Archive) {
                    1
                } else {
                    2
                };
                (rank, -main.max(archive), k.chat.id)
            })
            .collect();
        ids.sort_unstable();
        ids.into_iter().map(|(_, _, id)| id).collect()
    }

    /// The chat's kind as the store keeps it.
    pub(crate) fn kind(&self, chat_id: i64) -> Option<ChatKind> {
        self.chats.get(&chat_id).map(|k| self.chat_kind(&k.chat))
    }

    /// The conversation of a mapped chat.
    pub(crate) fn conv(&self, chat_id: i64) -> Option<ConvKey> {
        self.is_mapped(chat_id)
            .then(|| self.chats.get(&chat_id).and_then(|k| key(&k.chat)))
            .flatten()
    }

    /// The chat's patch to put before a message row (C6a): RESTATED with the chat's first row of
    /// this run, gated after it (critic L6). `None` for a chat that is not mapped.
    pub(crate) fn row_upsert(&mut self, chat_id: i64) -> Option<Event> {
        let k = self.chats.get(&chat_id)?;
        if !self.is_mapped(chat_id) {
            return None;
        }
        let patch = self.patch(k)?;
        // The patch rides this row: the flush has nothing left to state for the chat.
        self.dirty.remove(&chat_id);
        let gate = if self.restated.insert(chat_id) {
            Gate::Restate(chat_id)
        } else {
            Gate::Chat(chat_id)
        };
        Some(Event {
            event: BackendEvent::ConversationUpsert(patch),
            gate: Some(gate),
        })
    }

    fn room(&mut self) -> bool {
        if self.linked {
            return true;
        }
        let have =
            self.chats.len() + self.users.len() + self.supergroups.len() + self.basic_groups.len();
        if have < self.bound() {
            return true;
        }
        self.dropped += 1;
        false
    }

    fn learn_chat(&mut self, chat: Chat, out: &mut Out) {
        let id = chat.id;
        if let Some(k) = self.chats.get_mut(&id) {
            // Told again (a getChat answer, a restart's re-telling): its lists stay.
            let added = std::mem::take(&mut k.added);
            let has_rows = k.has_rows;
            let moved_to = k.moved_to;
            *k = Known {
                chat,
                added,
                has_rows,
                moved_to,
            };
        } else if self.room() {
            let mut k = Known::new(chat);
            if let ChatType::BasicGroup { basic_group_id } = k.chat.r#type {
                k.moved_to = self
                    .basic_groups
                    .get(&basic_group_id)
                    .and_then(upgraded_chat);
            }
            self.chats.insert(id, k);
            self.missing.remove(&id);
        } else {
            if self.missing.len() < self.bound() {
                self.missing.insert(id);
            }
            return;
        }
        self.stage(id, out);
    }

    fn learn_user(&mut self, user: TgUser, out: &mut Out) {
        let id = user.id;
        if !self.users.contains_key(&id) && !self.room() {
            return;
        }
        self.users.insert(id, user);
        if self.linked {
            self.dirty_users.insert(id);
        }
        if self.chats.contains_key(&id) {
            // A private chat's id is its user's: a bot's chat becomes held, an undecided one decided.
            self.stage(id, out);
        }
    }

    fn learn_supergroup(&mut self, sg: Supergroup, out: &mut Out) {
        let id = sg.id;
        if !self.supergroups.contains_key(&id) && !self.room() {
            return;
        }
        self.supergroups.insert(id, sg);
        let chat_id = ZERO_CHANNEL_ID - id;
        if self.chats.contains_key(&chat_id) {
            self.stage(chat_id, out);
        }
    }

    fn learn_basic_group(&mut self, bg: BasicGroup, out: &mut Out) {
        let id = bg.id;
        if !self.basic_groups.contains_key(&id) && !self.room() {
            return;
        }
        let moved_to = upgraded_chat(&bg);
        self.basic_groups.insert(id, bg);
        let chat_id = -id;
        if let Some(k) = self.chats.get_mut(&chat_id) {
            if moved_to.is_some() {
                k.moved_to = moved_to;
            }
            self.stage(chat_id, out);
        }
    }

    /// A fact about a chat: applied when the chat is known, else the chat is asked (H1).
    fn with_chat(&mut self, chat_id: i64, out: &mut Out, apply: impl FnOnce(&mut Known)) {
        match self.chats.get_mut(&chat_id) {
            Some(k) => {
                apply(k);
                self.stage(chat_id, out);
            }
            None => out.extend(self.reference(chat_id)),
        }
    }

    /// `getUser` for a user the machine does not know, once per id per run, only after the link
    /// (before it, the link's restaging asks).
    fn ask_user(&mut self, user_id: i64, out: &mut Out) {
        if self.linked
            && user_id > 0
            && self.me != Some(user_id)
            && !self.users.contains_key(&user_id)
            && self.asked_users.insert(user_id)
        {
            out.fetch.push(Fetch::User(user_id));
        }
    }

    fn ask(&mut self, chat_id: i64, out: &mut Out) {
        if self.chats.contains_key(&chat_id)
            || self.tries.get(&chat_id).copied() >= Some(CHAT_TRIES)
        {
            return;
        }
        if self.asked.insert(chat_id) {
            *self.tries.entry(chat_id).or_insert(0) += 1;
            out.fetch.push(Fetch::Chat(chat_id));
        }
    }

    /// `Ok(None)`: mapped; `Ok(Some(h))`: held; `Err(())`: undecided or never mapped.
    fn hold(&self, chat: &Chat) -> Result<Option<Held>, ()> {
        match chat.r#type {
            ChatType::Private { user_id } => match self.users.get(&user_id) {
                Some(u) if u.r#type == UserType::Bot => Ok(Some(Held::Bot)),
                Some(u) if restricted_info(u.restriction_info.as_ref()) => {
                    Ok(Some(Held::Restricted))
                }
                Some(_) => Ok(None),
                None => Err(()),
            },
            ChatType::BasicGroup { .. } => Ok(None),
            ChatType::Supergroup {
                supergroup_id,
                is_channel,
            } => {
                if is_channel {
                    return Ok(Some(Held::Channel));
                }
                let sg = self.supergroups.get(&supergroup_id).ok_or(())?;
                Ok(if sg.is_channel {
                    Some(Held::Channel)
                } else if sg.is_direct_messages_group {
                    Some(Held::DirectMessages)
                } else if sg
                    .usernames
                    .as_ref()
                    .is_some_and(|u| u.active_usernames.iter().any(|n| !n.is_empty()))
                {
                    Some(Held::Public)
                } else if restricted_info(sg.restriction_info.as_ref()) {
                    Some(Held::Restricted)
                } else {
                    None
                })
            }
            ChatType::Secret { .. } | ChatType::Unknown => Err(()),
        }
    }

    fn chat_kind(&self, chat: &Chat) -> ChatKind {
        match chat.r#type {
            ChatType::Private { .. } => ChatKind::Direct,
            ChatType::BasicGroup { .. } => ChatKind::Group,
            ChatType::Supergroup {
                supergroup_id,
                is_channel,
            } => {
                let sg = self.supergroups.get(&supergroup_id);
                if is_channel || sg.is_some_and(|s| s.is_channel) {
                    ChatKind::Channel
                } else if sg.is_some_and(|s| s.is_forum) {
                    ChatKind::Forum
                } else {
                    ChatKind::Supergroup
                }
            }
            ChatType::Secret { .. } | ChatType::Unknown => ChatKind::Unknown,
        }
    }

    /// The chat's conversation patch (brief §3.2; the module doc).
    fn patch(&self, k: &Known) -> Option<ConversationPatch> {
        let chat = &k.chat;
        let mut patch = ConversationPatch::new(key(chat)?);
        if let ChatType::Private { user_id } = chat.r#type
            && Some(user_id) == self.me
        {
            patch.is_self = Some(true);
        }
        if let Some(title) = plain_line(chat.title.as_str()) {
            patch.title = Patch::Set(title);
        }
        patch.archived = if k.in_one(Listed::Archive) {
            Some(true)
        } else if k.in_one(Listed::Main) {
            Some(false)
        } else {
            None
        };
        patch.muted_until = self.muted_until(chat);
        patch.marked_unread = Some(chat.is_marked_as_unread);
        patch.timer = match u64::try_from(chat.message_auto_delete_time) {
            Ok(secs) if secs > 0 => Patch::Set(Duration::from_secs(secs)),
            _ => Patch::Clear,
        };
        let rows = self.unread_rows.get(&chat.id).map_or(0, |ids| {
            ids.range(chat.last_read_inbox_message_id.saturating_add(1)..)
                .count()
        });
        let count = usize::try_from(chat.unread_count).unwrap_or(0);
        patch.unread_hint = Some(u32::try_from(count.saturating_sub(rows)).unwrap_or(u32::MAX));
        patch.meta = Patch::Set(meta(&serde_json::json!({
            "protected": chat.has_protected_content,
            "restricted": self.restricted(chat),
        })));
        patch.features = Some(features());
        Some(patch)
    }

    /// `muted_until` (ms): the chat's own `mute_for`, or its scope's when it takes the default;
    /// over [`MUTE_FOREVER_SECS`] until unmuted; 0 clears. Whole seconds, so a re-telling a
    /// moment later states the same value.
    fn muted_until(&self, chat: &Chat) -> Patch<i64> {
        let settings = chat.notification_settings;
        let mute_for = if settings.use_default_mute_for {
            ScopeKey::of(scope_of(chat))
                .and_then(|s| self.scope_mute.get(&s).copied())
                .unwrap_or(0)
        } else {
            settings.mute_for
        };
        let mute_for = i64::from(mute_for);
        if mute_for <= 0 {
            Patch::Clear
        } else if mute_for > MUTE_FOREVER_SECS {
            Patch::Set(i64::MAX)
        } else {
            Patch::Set((self.now_ms.div_euclid(1000) + mute_for).saturating_mul(1000))
        }
    }

    /// The chat's user or supergroup carries a restriction reason (also a hold since TF13).
    fn restricted(&self, chat: &Chat) -> bool {
        let info = match chat.r#type {
            ChatType::Private { user_id } => self
                .users
                .get(&user_id)
                .and_then(|u| u.restriction_info.as_ref()),
            ChatType::Supergroup { supergroup_id, .. } => self
                .supergroups
                .get(&supergroup_id)
                .and_then(|s| s.restriction_info.as_ref()),
            _ => None,
        };
        info.is_some_and(|r| !r.restriction_reason.is_empty())
    }

    /// The patch of a listed, mapped conversation.
    fn listed_patch(&self, chat_id: i64) -> Option<ConversationPatch> {
        let k = self.chats.get(&chat_id)?;
        let listed = k.in_list() || k.has_rows;
        match self.hold(&k.chat) {
            Ok(None) if listed => self.patch(k),
            _ => None,
        }
    }

    /// Kourier needs this user: a listed private chat's peer, a contact, a message's author;
    /// never the own account.
    fn needs_user(&self, user_id: i64) -> bool {
        if Some(user_id) == self.me {
            return false;
        }
        self.authors.contains(&user_id)
            || self.users.get(&user_id).is_some_and(|u| u.is_contact)
            || self.listed_patch(user_id).is_some()
    }

    /// The user's gated contact patch, `None` for the own account or an unknown user.
    fn user_event(&self, user_id: i64) -> Option<Event> {
        if Some(user_id) == self.me {
            return None;
        }
        let user = self.users.get(&user_id)?;
        let id = tg_user(user_id).filter(|_| user_id > 0)?;
        Some(Event {
            event: BackendEvent::ContactUpsert(contact_patch(user, &id)),
            gate: Some(Gate::User(user_id)),
        })
    }

    /// The main-list pins of listed, mapped conversations, highest order first.
    fn pinned(&self) -> Vec<ConvKey> {
        let mut pins: Vec<(i64, i64, ConvKey)> = self
            .chats
            .values()
            .filter_map(|k| {
                let order = k.main_pin()?;
                let key = self.listed_patch(k.chat.id)?.key;
                Some((order, k.chat.id, key))
            })
            .collect();
        pins.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        pins.into_iter().map(|(_, _, key)| key).collect()
    }

    /// After the link: the chat's changed facts at once; its patch at the next flush.
    fn stage(&mut self, chat_id: i64, out: &mut Out) {
        if !self.linked {
            return;
        }
        let Some(k) = self.chats.get(&chat_id) else {
            return;
        };
        let hold = self.hold(&k.chat);
        let unknown_user = match k.chat.r#type {
            ChatType::Private { user_id } if hold.is_err() => Some(user_id),
            _ => None,
        };
        let group = match k.chat.r#type {
            ChatType::Supergroup {
                supergroup_id,
                is_channel: false,
            } if hold.is_err() => Some(Group::Super(supergroup_id)),
            ChatType::BasicGroup { basic_group_id } => Some(Group::Basic(basic_group_id)),
            _ => None,
        };
        let listed = k.in_list() || k.has_rows;
        let facts = ChatFacts {
            chat_id,
            kind: Some(self.chat_kind(&k.chat)),
            listed: Some(listed),
            held: hold.ok(),
            moved_to: k.moved_to,
            ..ChatFacts::default()
        };
        if self.facts.get(&chat_id) != Some(&facts) {
            self.facts.insert(chat_id, facts.clone());
            out.staged.chats.push(facts);
        }
        self.dirty.insert(chat_id);
        // A private chat undecided for want of its user (Q-B1).
        if let Some(user_id) = unknown_user {
            self.ask_user(user_id, out);
        }
        // A supergroup chat undecided for want of its supergroup, a basic group chat without
        // its object (TF4): TDLib answers both from memory and never tells them twice.
        match group {
            Some(Group::Super(id))
                if !self.supergroups.contains_key(&id) && self.asked_supergroups.insert(id) =>
            {
                out.fetch.push(Fetch::Supergroup(id));
            }
            Some(Group::Basic(id))
                if !self.basic_groups.contains_key(&id) && self.asked_basic_groups.insert(id) =>
            {
                out.fetch.push(Fetch::BasicGroup(id));
            }
            _ => {}
        }
    }
}

/// A restriction info names a reason (TF13).
fn restricted_info(info: Option<&crate::tdapi::RestrictionInfo>) -> bool {
    info.is_some_and(|r| !r.restriction_reason.is_empty())
}

/// A group object a chat needs and may not have (TF4).
#[derive(Clone, Copy)]
enum Group {
    Super(i64),
    Basic(i64),
}

/// The supergroup chat a basic group moved to.
fn upgraded_chat(bg: &BasicGroup) -> Option<i64> {
    (bg.upgraded_to_supergroup_id != 0).then(|| ZERO_CHANNEL_ID - bg.upgraded_to_supergroup_id)
}

/// The TDLib chat id of a Telegram conversation key (the inverse of [`key`]: a private chat's id
/// is its user's, a group's the signed chat id), `None` for a key of another shape.
pub(crate) fn chat_id_of(conv: &ConvKey) -> Option<i64> {
    match conv {
        ConvKey::Direct(user) if user.scheme() == "tg" => {
            user.value().parse::<i64>().ok().filter(|&id| id > 0)
        }
        ConvKey::Group(key) | ConvKey::Channel(key) => key
            .strip_prefix("tg:")
            .and_then(|id| id.parse::<i64>().ok())
            .filter(|&id| id < 0),
        ConvKey::Direct(_) | ConvKey::Broadcast(_) => None,
    }
}

/// The conversation key of a chat (brief §3.1): a private chat is its user's `Direct`, a basic or
/// super group `Group("tg:<signed chat id>")`, a channel `Channel(..)` (held anyway).
fn key(chat: &Chat) -> Option<ConvKey> {
    match chat.r#type {
        ChatType::Private { user_id } if user_id > 0 => {
            UserId::from_parts("tg", &user_id.to_string())
                .ok()
                .map(ConvKey::Direct)
        }
        ChatType::BasicGroup { .. }
        | ChatType::Supergroup {
            is_channel: false, ..
        } if chat.id < 0 => Some(ConvKey::Group(format!("tg:{}", chat.id).into())),
        ChatType::Supergroup {
            is_channel: true, ..
        } if chat.id < 0 => Some(ConvKey::Channel(format!("tg:{}", chat.id).into())),
        _ => None,
    }
}
