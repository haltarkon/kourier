//! `SearchListModel`: the hits of a message search (G1m), `App.searchResults`. Rows are the
//! viewmodel's [`SearchRow`]s keyed by the message and updated through the diff engine
//! (`diff_or_reset`, as `MemberListModel`), so a re-read that drops one deleted message is one
//! `rowsRemoved` and the rows around it stay.
//!
//! The view's other fields are properties of the model itself (not of `App`): `active`, `chat`,
//! `more`, `loading`, `indexReady`, `indexPercent`, `capped` and `answered`. They change together with the
//! rows, in [`qobject::SearchListModel::show`], and a view of an older generation changes
//! nothing of either.
//!
//! The snippet is two roles: `snippetHtml` is escaped markup for a `Text.StyledText` label (the
//! only markup is `<b>` around the matches), `snippetPlain` is the same text without markup (the
//! accessible name). Nothing of a hit's text is ever given to a label that guesses its format.

use cxx_qt_lib::{QHash, QHashPair_i32_QByteArray, QModelIndex, QString, QVariant};
use kourier_model::{ConvKind, MessageId};
use kourier_viewmodel::diff::RoleSet;
use kourier_viewmodel::search::{SearchRow, SearchView};

use super::{ListModel, Store, list_model_glue, role_hash, role_index, row_at, set_rows};
use crate::bridge::qobject;

/// Role names, in role-index order.
pub const ROLES: [&str; 20] = [
    "message",
    "conversationId",
    "accountId",
    "title",
    "avatarUrl",
    "author",
    "snippetHtml",
    "snippetPlain",
    "icon",
    "timeLabel",
    "sentAt",
    "archived",
    "accountBadge",
    "badgeIcon",
    "badgeNumber",
    "isGroup",
    "isSelf",
    "tint",
    "senderName",
    "senderAvatarUrl",
];

/// Rust side of `SearchListModel`.
pub struct SearchRust {
    pub(crate) count: i32,
    /// A message search runs.
    pub(crate) active: bool,
    /// The one chat the search is limited to (0: every chat).
    pub(crate) chat: i64,
    /// More hits follow the last row.
    pub(crate) more: bool,
    /// An answer is on its way.
    pub(crate) loading: bool,
    /// Every message is indexed.
    pub(crate) index_ready: bool,
    /// How much of the old messages is indexed (100 when ready).
    pub(crate) index_percent: i32,
    /// The last answer stopped early: an empty list is not "no results".
    pub(crate) capped: bool,
    /// The core answered this search: an empty list is "No results" unless `capped`.
    pub(crate) answered: bool,
    /// The chat of a one-chat search left the list.
    pub(crate) chat_gone: bool,
    /// The core refused the pattern: the words under the field.
    pub(crate) refusal: QString,
    pub(crate) store: Store<SearchRow, SearchQt>,
}

impl Default for SearchRust {
    fn default() -> Self {
        Self {
            count: 0,
            active: false,
            chat: 0,
            more: false,
            loading: false,
            // The view's own default: nothing is waiting for the index until an answer says so.
            index_ready: true,
            index_percent: 100,
            capped: false,
            answered: false,
            chat_gone: false,
            refusal: QString::default(),
            store: Store::default(),
        }
    }
}

/// One hit as Qt values.
pub struct SearchQt {
    message: i64,
    conversation: i64,
    account: i64,
    title: QString,
    avatar_url: QString,
    author: QString,
    snippet_html: QString,
    snippet_plain: QString,
    icon: QString,
    time_label: QString,
    sent_at: i64,
    archived: bool,
    account_badge: QString,
    badge_icon: QString,
    badge_number: i32,
    is_group: bool,
    is_self: bool,
    tint: QString,
    sender_name: QString,
    sender_avatar_url: QString,
}

impl SearchQt {
    fn from_view(v: &SearchRow) -> Self {
        Self {
            message: v.message.0,
            conversation: v.conv.0,
            account: v.account.0,
            title: QString::from(v.title.as_str()),
            avatar_url: QString::from(v.avatar_url.as_str()),
            author: QString::from(v.author.as_str()),
            snippet_html: QString::from(v.snippet.html.as_str()),
            snippet_plain: QString::from(v.snippet.plain.as_str()),
            icon: QString::from(v.icon.as_str()),
            time_label: QString::from(v.time_label.as_str()),
            sent_at: v.sent_at,
            archived: v.archived,
            account_badge: QString::from(v.account_badge.as_str()),
            badge_icon: QString::from(v.badge_icon.as_str()),
            badge_number: i32::try_from(v.badge_number).unwrap_or(i32::MAX),
            is_group: v.kind != ConvKind::Direct,
            is_self: v.is_self,
            tint: QString::from(v.tint.as_str()),
            sender_name: QString::from(v.sender.as_str()),
            sender_avatar_url: QString::from(v.sender_avatar_url.as_str()),
        }
    }

    /// The roles whose values differ (the row's message is its key, so role 0 never does).
    fn changed_roles(a: &SearchRow, b: &SearchRow) -> RoleSet {
        RoleSet::EMPTY
            .with_if(1, a.conv != b.conv)
            .with_if(2, a.account != b.account)
            .with_if(3, a.title != b.title)
            .with_if(4, a.avatar_url != b.avatar_url)
            .with_if(5, a.author != b.author)
            .with_if(6, a.snippet.html != b.snippet.html)
            .with_if(7, a.snippet.plain != b.snippet.plain)
            .with_if(8, a.icon != b.icon)
            .with_if(9, a.time_label != b.time_label)
            .with_if(10, a.sent_at != b.sent_at)
            .with_if(11, a.archived != b.archived)
            .with_if(12, a.account_badge != b.account_badge)
            .with_if(13, a.badge_icon != b.badge_icon)
            .with_if(14, a.badge_number != b.badge_number)
            .with_if(15, a.kind != b.kind)
            .with_if(16, a.is_self != b.is_self)
            .with_if(17, a.tint != b.tint)
            .with_if(18, a.sender != b.sender)
            .with_if(19, a.sender_avatar_url != b.sender_avatar_url)
    }

    fn key(v: &SearchRow) -> MessageId {
        v.message
    }
}

impl qobject::SearchListModel {
    /// `QAbstractItemModel::data`.
    pub fn data(&self, index: &QModelIndex, role: i32) -> QVariant {
        let (Some(row), Some(role)) = (row_at(&self.store, index), role_index(role, ROLES.len()))
        else {
            return QVariant::default();
        };
        match role {
            0 => QVariant::from(&row.message),
            1 => QVariant::from(&row.conversation),
            2 => QVariant::from(&row.account),
            3 => QVariant::from(&row.title),
            4 => QVariant::from(&row.avatar_url),
            5 => QVariant::from(&row.author),
            6 => QVariant::from(&row.snippet_html),
            7 => QVariant::from(&row.snippet_plain),
            8 => QVariant::from(&row.icon),
            9 => QVariant::from(&row.time_label),
            10 => QVariant::from(&row.sent_at),
            11 => QVariant::from(&row.archived),
            12 => QVariant::from(&row.account_badge),
            13 => QVariant::from(&row.badge_icon),
            14 => QVariant::from(&row.badge_number),
            15 => QVariant::from(&row.is_group),
            16 => QVariant::from(&row.is_self),
            17 => QVariant::from(&row.tint),
            18 => QVariant::from(&row.sender_name),
            _ => QVariant::from(&row.sender_avatar_url),
        }
    }

    /// `QAbstractItemModel::rowCount`.
    pub fn row_count(&self, parent: &QModelIndex) -> i32 {
        if parent.is_valid() {
            0
        } else {
            i32::try_from(self.store.qt.len()).unwrap_or(i32::MAX)
        }
    }

    /// `QAbstractItemModel::roleNames`.
    pub fn role_names(&self) -> QHash<QHashPair_i32_QByteArray> {
        role_hash(&ROLES)
    }

    /// The message of listed row `row` (0 when there is none).
    pub fn message_at(&self, row: i32) -> i64 {
        usize::try_from(row)
            .ok()
            .and_then(|r| self.store.qt_row(r))
            .map_or(0, |r| r.message)
    }

    /// The chat of listed row `row` (0 when there is none).
    pub fn conversation_at(&self, row: i32) -> i64 {
        usize::try_from(row)
            .ok()
            .and_then(|r| self.store.qt_row(r))
            .map_or(0, |r| r.conversation)
    }

    /// Takes the search view of generation `generation`: the rows go through the diff and the
    /// properties follow. A view older than the rows shown changes nothing and gives `false`.
    pub fn show(mut self: std::pin::Pin<&mut Self>, generation: u64, view: SearchView) -> bool {
        if generation < self.store().generation {
            return false;
        }
        let SearchView {
            active,
            chat,
            rows,
            more,
            loading,
            index_ready,
            index_percent,
            capped,
            answered,
            chat_gone,
            refusal,
        } = view;
        set_rows(self.as_mut(), generation, rows);
        // (Each setter notifies only when the value changed.)
        self.as_mut().set_active(active);
        self.as_mut().set_chat(chat.map_or(0, |c| c.0));
        self.as_mut().set_more(more);
        self.as_mut().set_loading(loading);
        self.as_mut().set_index_ready(index_ready);
        self.as_mut().set_index_percent(i32::from(index_percent));
        self.as_mut().set_capped(capped);
        self.as_mut().set_answered(answered);
        self.as_mut().set_chat_gone(chat_gone);
        self.as_mut().set_refusal(QString::from(refusal.as_str()));
        true
    }
}

list_model_glue!(qobject::SearchListModel, SearchRow, SearchQt, MessageId);
