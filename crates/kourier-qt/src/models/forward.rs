//! `ForwardTargetListModel`: the chats of the forward dialog (forward line F6, "Forward…"),
//! `App.forwardTargets`. Rows are [`ForwardTarget`]s keyed by the conversation id and updated
//! through the diff engine (`diff_or_reset`, as `ShareTargetListModel`). Which chats are listed
//! (the shown accounts, the chats that can send, Note to Self included, the archive after the
//! inbox), which of them are on the source's account (`native`) and the search are the
//! viewmodel's ([`listed_forward_targets`]); this model only holds the candidates, the account of
//! the source, the shown accounts and the search text. No row holds anything of the forward: the
//! core plans it and the dialog's ticks are QML's own.

use std::pin::Pin;

use cxx_qt::CxxQtType;
use cxx_qt_lib::{QHash, QHashPair_i32_QByteArray, QModelIndex, QString, QVariant};
use kourier_model::{AccountId, ConversationId};
use kourier_viewmodel::diff::RoleSet;
use kourier_viewmodel::forward::{ForwardTarget, listed_forward_targets};

use super::{ListModel, Store, list_model_glue, role_hash, role_index, row_at, set_rows};
use crate::bridge::qobject;

const ROLES: [&str; 7] = [
    "conversation",
    "title",
    "avatarUrl",
    "isSelf",
    "accountTag",
    "tint",
    "sameAccount",
];

/// Rust side of `ForwardTargetListModel`.
pub struct ForwardTargetsRust {
    pub(crate) count: i32,
    /// The text of the dialog's search; chats whose title does not contain it are not listed.
    pub(crate) filter: QString,
    /// The account of the chat the messages come from (`native` is judged against it).
    pub(crate) source_account: AccountId,
    /// The accounts the chat list shows: only their chats are listed.
    pub(crate) shown: Vec<AccountId>,
    /// Every chat of the list the source holds (before the accounts and the search).
    pub(crate) all: Vec<ForwardTarget>,
    pub(crate) store: Store<ForwardTarget, ForwardTargetQt>,
}

impl Default for ForwardTargetsRust {
    fn default() -> Self {
        Self {
            count: 0,
            filter: QString::default(),
            source_account: AccountId(0),
            shown: Vec::new(),
            all: Vec::new(),
            store: Store::default(),
        }
    }
}

/// One chat row as Qt values.
pub struct ForwardTargetQt {
    conversation: i64,
    title: QString,
    avatar_url: QString,
    is_self: bool,
    account_tag: QString,
    tint: QString,
    native: bool,
}

impl ForwardTargetQt {
    fn from_view(v: &ForwardTarget) -> Self {
        Self {
            conversation: v.id.0,
            title: QString::from(v.title.as_str()),
            avatar_url: QString::from(v.avatar_url.as_str()),
            is_self: v.is_self,
            account_tag: QString::from(v.account_tag.as_str()),
            tint: QString::from(v.tint.as_str()),
            native: v.native,
        }
    }

    fn changed_roles(a: &ForwardTarget, b: &ForwardTarget) -> RoleSet {
        ForwardTarget::changed_roles(a, b)
    }

    fn key(v: &ForwardTarget) -> ConversationId {
        v.key()
    }
}

impl qobject::ForwardTargetListModel {
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
            3 => QVariant::from(&row.is_self),
            4 => QVariant::from(&row.account_tag),
            5 => QVariant::from(&row.tint),
            _ => QVariant::from(&row.native),
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
            .and_then(|r| self.store.qt_row(r))
            .map_or(0, |r| r.conversation)
    }

    /// The title of listed chat `conversation` (empty when it is not listed).
    pub fn title_of(&self, conversation: i64) -> QString {
        self.store
            .qt_rows()
            .find(|r| r.conversation == conversation)
            .map(|r| r.title.clone())
            .unwrap_or_default()
    }

    /// Whether chat `conversation` is listed now.
    pub fn is_listed(&self, conversation: i64) -> bool {
        self.store.view.iter().any(|t| t.id.0 == conversation)
    }

    /// Whether listed chat `conversation` is on the source's account (the network's own forward).
    pub fn is_native(&self, conversation: i64) -> bool {
        self.store
            .view
            .iter()
            .any(|t| t.id.0 == conversation && t.native)
    }

    /// The dialog's search changed (`filter` is writable from QML): the listed rows follow.
    pub fn set_filter_text(mut self: Pin<&mut Self>, text: &QString) {
        if self.filter == *text {
            return;
        }
        self.as_mut().rust_mut().filter = text.clone();
        self.as_mut().filter_changed();
        self.reapply();
    }

    /// Takes the candidates, the source's account and the shown accounts; nothing changes when
    /// they are what is held.
    pub fn set_targets(
        mut self: Pin<&mut Self>,
        source_account: AccountId,
        shown: Vec<AccountId>,
        all: Vec<ForwardTarget>,
    ) {
        if self.source_account == source_account && self.shown == shown && self.all == all {
            return;
        }
        {
            let mut rust = self.as_mut().rust_mut();
            rust.source_account = source_account;
            rust.shown = shown;
            rust.all = all;
        }
        self.reapply();
    }

    /// The listed rows again from `all`, the accounts and the filter, through the diff.
    fn reapply(mut self: Pin<&mut Self>) {
        let rows = listed_forward_targets(
            &self.all,
            &self.shown,
            self.source_account,
            &self.filter.to_string(),
        );
        let generation = self.store().generation + 1;
        set_rows(self.as_mut(), generation, rows);
    }
}

list_model_glue!(
    qobject::ForwardTargetListModel,
    ForwardTarget,
    ForwardTargetQt,
    ConversationId
);
