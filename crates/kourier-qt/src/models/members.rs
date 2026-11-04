//! `MemberListModel`: the member rows of the group's page (people pages P2b). `App` owns two:
//! `groupMembers` (every listed member, narrowed by its `filter`) and `groupPreview` (the first
//! rows the group level shows). Rows are `GroupMemberRow`s keyed by the member's user id and
//! updated through the diff engine (`diff_or_reset`), so a refresh that renames one member is one
//! `dataChanged` of the `name` role and a small change is inserts, removes and moves. The list resets
//! when either side is empty (first fill, no match, an empty page) or when more than 16 rows and
//! half of them change (the first letter typed into a 1,000-member search).

use std::pin::Pin;

use cxx_qt::CxxQtType;
use cxx_qt_lib::{QHash, QHashPair_i32_QByteArray, QModelIndex, QString, QVariant};
use kourier_model::UserId;
use kourier_viewmodel::diff::RoleSet;
use kourier_viewmodel::group::{GroupMemberRow, listed_members};

use super::{ListModel, Store, list_model_glue, role_hash, role_index, row_at, set_rows};
use crate::bridge::qobject;

const ROLES: [&str; 5] = ["user", "name", "avatarUrl", "admin", "isSelf"];

/// Rust side of `MemberListModel`.
#[derive(Default)]
pub struct MembersRust {
    pub(crate) count: i32,
    /// The text of the members search; rows that do not match it are not listed.
    pub(crate) filter: QString,
    /// Every member of the page, in the page's order (the listed rows are the ones that match
    /// `filter`).
    pub(crate) all: Vec<GroupMemberRow>,
    pub(crate) store: Store<GroupMemberRow, MemberQt>,
}

/// One member row as Qt values.
pub struct MemberQt {
    user: QString,
    name: QString,
    avatar_url: QString,
    admin: bool,
    is_self: bool,
}

impl MemberQt {
    fn from_view(v: &GroupMemberRow) -> Self {
        Self {
            user: QString::from(v.user.as_str()),
            name: QString::from(v.name.as_str()),
            avatar_url: QString::from(v.avatar_url.as_str()),
            admin: v.admin,
            is_self: v.is_self,
        }
    }

    fn changed_roles(a: &GroupMemberRow, b: &GroupMemberRow) -> RoleSet {
        GroupMemberRow::changed_roles(a, b)
    }

    fn key(v: &GroupMemberRow) -> UserId {
        v.key()
    }
}

impl qobject::MemberListModel {
    /// `QAbstractItemModel::data`.
    pub fn data(&self, index: &QModelIndex, role: i32) -> QVariant {
        let (Some(row), Some(role)) = (row_at(&self.store, index), role_index(role, ROLES.len()))
        else {
            return QVariant::default();
        };
        match role {
            0 => QVariant::from(&row.user),
            1 => QVariant::from(&row.name),
            2 => QVariant::from(&row.avatar_url),
            3 => QVariant::from(&row.admin),
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

    /// The user id of listed row `row` (empty when there is none).
    pub fn user_at(&self, row: i32) -> QString {
        usize::try_from(row)
            .ok()
            .and_then(|r| self.store.qt.get(r))
            .map(|r| r.user.clone())
            .unwrap_or_default()
    }

    /// The listed row of member `user` (-1 when the member is not listed).
    pub fn row_of(&self, user: &QString) -> i32 {
        let user = user.to_string();
        self.store
            .view
            .iter()
            .position(|m| m.user.as_str() == user)
            .and_then(|r| i32::try_from(r).ok())
            .unwrap_or(-1)
    }

    /// The members search changed (`filter` is writable from QML): the listed rows follow.
    pub fn set_filter_text(mut self: Pin<&mut Self>, text: &QString) {
        if self.filter == *text {
            return;
        }
        self.as_mut().rust_mut().filter = text.clone();
        self.as_mut().filter_changed();
        self.reapply();
    }

    /// Takes the page's members (in the page's order) and lists the ones the filter lets
    /// through; nothing changes when they are the rows already held.
    pub fn set_members(mut self: Pin<&mut Self>, all: Vec<GroupMemberRow>) {
        if self.all == all {
            return;
        }
        self.as_mut().rust_mut().all = all;
        self.reapply();
    }

    /// The listed rows again from `all` and the filter, through the diff.
    fn reapply(mut self: Pin<&mut Self>) {
        let rows = listed_members(&self.all, &self.filter.to_string());
        let generation = self.store().generation + 1;
        set_rows(self.as_mut(), generation, rows);
    }
}

list_model_glue!(qobject::MemberListModel, GroupMemberRow, MemberQt, UserId);
