//! `ShareTargetListModel`: the chats of the share picker (people pages P5c, "Share Contact…"),
//! `App.shareTargets`. Rows are [`ShareTarget`]s keyed by the conversation id and updated through
//! the diff engine (`diff_or_reset`, as `MemberListModel`). Which chats are listed (the person's
//! account, the chats that can send, Note to Self included, the archive after the inbox) and the
//! search are the viewmodel's ([`listed_targets`]); this model only holds the candidates, the
//! account and the search text. No row holds anything of the card: the core builds it.

use std::pin::Pin;

use cxx_qt::CxxQtType;
use cxx_qt_lib::{QHash, QHashPair_i32_QByteArray, QModelIndex, QString, QVariant};
use kourier_model::{AccountId, ConversationId};
use kourier_viewmodel::diff::RoleSet;
use kourier_viewmodel::share::{ShareTarget, listed_targets};

use super::{ListModel, Store, list_model_glue, role_hash, role_index, row_at, set_rows};
use crate::bridge::qobject;

const ROLES: [&str; 4] = ["conversation", "title", "avatarUrl", "isSelf"];

/// Rust side of `ShareTargetListModel`.
pub struct ShareTargetsRust {
    pub(crate) count: i32,
    /// The text of the picker's search; chats whose title does not contain it are not listed.
    pub(crate) filter: QString,
    /// The account of the person whose card is shared: only its chats are listed.
    pub(crate) account: AccountId,
    /// Every chat of the list the source holds (before the account and the search).
    pub(crate) all: Vec<ShareTarget>,
    pub(crate) store: Store<ShareTarget, ShareTargetQt>,
}

impl Default for ShareTargetsRust {
    fn default() -> Self {
        Self {
            count: 0,
            filter: QString::default(),
            // No person yet: account 0 has no chats, so nothing is listed.
            account: AccountId(0),
            all: Vec::new(),
            store: Store::default(),
        }
    }
}

/// One chat row as Qt values.
pub struct ShareTargetQt {
    conversation: i64,
    title: QString,
    avatar_url: QString,
    is_self: bool,
}

impl ShareTargetQt {
    fn from_view(v: &ShareTarget) -> Self {
        Self {
            conversation: v.id.0,
            title: QString::from(v.title.as_str()),
            avatar_url: QString::from(v.avatar_url.as_str()),
            is_self: v.is_self,
        }
    }

    fn changed_roles(a: &ShareTarget, b: &ShareTarget) -> RoleSet {
        ShareTarget::changed_roles(a, b)
    }

    fn key(v: &ShareTarget) -> ConversationId {
        v.key()
    }
}

impl qobject::ShareTargetListModel {
    /// `QAbstractItemModel::data`.
    pub fn data(&self, index: &QModelIndex, role: i32) -> QVariant {
        let (Some(row), Some(role)) = (row_at(&self.store, index), role_index(role, ROLES.len()))
        else {
            return QVariant::default();
        };
        match role {
            0 => QVariant::from(&row.conversation),
            1 => QVariant::from(&row.title),
            2 => QVariant::from(&row.avatar_url),
            _ => QVariant::from(&row.is_self),
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

    /// The conversation id of listed row `row` (0 when there is none).
    pub fn conversation_at(&self, row: i32) -> i64 {
        usize::try_from(row)
            .ok()
            .and_then(|r| self.store.qt.get(r))
            .map_or(0, |r| r.conversation)
    }

    /// The title of listed chat `conversation` (empty when it is not listed).
    pub fn title_of(&self, conversation: i64) -> QString {
        self.store
            .qt
            .iter()
            .find(|r| r.conversation == conversation)
            .map(|r| r.title.clone())
            .unwrap_or_default()
    }

    /// Whether chat `conversation` is listed now.
    pub fn is_listed(&self, conversation: i64) -> bool {
        self.store.view.iter().any(|t| t.id.0 == conversation)
    }

    /// The picker's search changed (`filter` is writable from QML): the listed rows follow.
    pub fn set_filter_text(mut self: Pin<&mut Self>, text: &QString) {
        if self.filter == *text {
            return;
        }
        self.as_mut().rust_mut().filter = text.clone();
        self.as_mut().filter_changed();
        self.reapply();
    }

    /// Takes the account of the person whose card is shared and every chat of the list, and
    /// lists the ones a card may go to; nothing changes when they are what is held.
    pub fn set_targets(mut self: Pin<&mut Self>, account: AccountId, all: Vec<ShareTarget>) {
        if self.account == account && self.all == all {
            return;
        }
        self.as_mut().rust_mut().account = account;
        self.as_mut().rust_mut().all = all;
        self.reapply();
    }

    /// The listed rows again from `all`, the account and the filter, through the diff.
    fn reapply(mut self: Pin<&mut Self>) {
        let rows = listed_targets(&self.all, self.account, &self.filter.to_string());
        let generation = self.store().generation + 1;
        set_rows(self.as_mut(), generation, rows);
    }
}

list_model_glue!(
    qobject::ShareTargetListModel,
    ShareTarget,
    ShareTargetQt,
    ConversationId
);
