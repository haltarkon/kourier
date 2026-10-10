//! `ConversationListModel`: the conversation list, sorted and filtered in Rust.

use cxx_qt_lib::{QHash, QHashPair_i32_QByteArray, QModelIndex, QString, QVariant};
use kourier_model::{ConvKind, ConversationId};
use kourier_viewmodel::ConversationRow;
use kourier_viewmodel::diff::RoleSet;

use super::{Store, list_model_glue, role_hash, role_index, row_at};
use crate::bridge::qobject;

/// Role names, in role-index order.
pub const ROLES: [&str; 20] = [
    "conversationId",
    "accountId",
    "title",
    "avatarUrl",
    "previewAuthor",
    "preview",
    "previewIcon",
    "typing",
    "timeLabel",
    "unread",
    "mentions",
    "markedUnread",
    "muted",
    "pinned",
    "receipt",
    "accountBadge",
    "badgeIcon",
    "isGroup",
    "isSelf",
    "previewSource",
];

/// Rust side of `ConversationListModel`.
#[derive(Default)]
pub struct ConversationsRust {
    pub(crate) count: i32,
    pub(crate) current_id: i64,
    pub(crate) store: Store<ConversationRow, ConversationQt>,
}

/// One conversation row as Qt values.
pub struct ConversationQt {
    id: i64,
    account: i64,
    title: QString,
    avatar_url: QString,
    preview_author: QString,
    preview: QString,
    preview_icon: QString,
    typing: QString,
    time_label: QString,
    unread: i32,
    mentions: i32,
    marked_unread: bool,
    muted: bool,
    pinned: bool,
    receipt: QString,
    account_badge: QString,
    badge_icon: QString,
    is_group: bool,
    is_self: bool,
    preview_source: QString,
}

fn n(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

impl ConversationQt {
    fn from_view(v: &ConversationRow) -> Self {
        Self {
            id: v.id.0,
            account: v.account.0,
            title: QString::from(v.title.as_str()),
            avatar_url: QString::from(v.avatar_url.as_str()),
            preview_author: QString::from(v.preview_author.as_str()),
            preview: QString::from(v.preview.as_str()),
            preview_icon: QString::from(v.preview_icon.as_str()),
            typing: QString::from(v.typing.as_str()),
            time_label: QString::from(v.time_label.as_str()),
            unread: n(v.unread),
            mentions: n(v.mentions),
            marked_unread: v.marked_unread,
            muted: v.muted,
            pinned: v.pinned(),
            receipt: QString::from(v.receipt.as_str()),
            account_badge: QString::from(v.account_badge.as_str()),
            badge_icon: QString::from(v.badge_icon.as_str()),
            is_group: v.kind != ConvKind::Direct,
            is_self: v.is_self,
            preview_source: QString::from(v.preview_source.as_str()),
        }
    }

    fn changed_roles(a: &ConversationRow, b: &ConversationRow) -> RoleSet {
        RoleSet::EMPTY
            .with_if(1, a.account != b.account)
            .with_if(2, a.title != b.title)
            .with_if(3, a.avatar_url != b.avatar_url)
            .with_if(4, a.preview_author != b.preview_author)
            .with_if(5, a.preview != b.preview)
            .with_if(6, a.preview_icon != b.preview_icon)
            .with_if(7, a.typing != b.typing)
            .with_if(8, a.time_label != b.time_label)
            .with_if(9, a.unread != b.unread)
            .with_if(10, a.mentions != b.mentions)
            .with_if(11, a.marked_unread != b.marked_unread)
            .with_if(12, a.muted != b.muted)
            .with_if(13, a.pinned_order.is_some() != b.pinned_order.is_some())
            .with_if(14, a.receipt != b.receipt)
            .with_if(15, a.account_badge != b.account_badge)
            .with_if(16, a.badge_icon != b.badge_icon)
            .with_if(17, a.kind != b.kind)
            .with_if(18, a.is_self != b.is_self)
            .with_if(19, a.preview_source != b.preview_source)
    }

    fn key(v: &ConversationRow) -> ConversationId {
        v.id
    }
}

impl qobject::ConversationListModel {
    /// `QAbstractItemModel::data`.
    pub fn data(&self, index: &QModelIndex, role: i32) -> QVariant {
        let (Some(r), Some(role)) = (row_at(&self.store, index), role_index(role, ROLES.len()))
        else {
            return QVariant::default();
        };
        match role {
            0 => QVariant::from(&r.id),
            1 => QVariant::from(&r.account),
            2 => QVariant::from(&r.title),
            3 => QVariant::from(&r.avatar_url),
            4 => QVariant::from(&r.preview_author),
            5 => QVariant::from(&r.preview),
            6 => QVariant::from(&r.preview_icon),
            7 => QVariant::from(&r.typing),
            8 => QVariant::from(&r.time_label),
            9 => QVariant::from(&r.unread),
            10 => QVariant::from(&r.mentions),
            11 => QVariant::from(&r.marked_unread),
            12 => QVariant::from(&r.muted),
            13 => QVariant::from(&r.pinned),
            14 => QVariant::from(&r.receipt),
            15 => QVariant::from(&r.account_badge),
            16 => QVariant::from(&r.badge_icon),
            17 => QVariant::from(&r.is_group),
            18 => QVariant::from(&r.is_self),
            _ => QVariant::from(&r.preview_source),
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

    /// The viewmodel rows currently shown (tests, App).
    pub fn rows(&self) -> &[ConversationRow] {
        &self.store.view
    }
}

list_model_glue!(
    qobject::ConversationListModel,
    ConversationRow,
    ConversationQt,
    ConversationId
);
