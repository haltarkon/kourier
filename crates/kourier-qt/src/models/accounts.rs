//! `AccountListModel`: the accounts of the drawer.

use cxx_qt_lib::{QHash, QHashPair_i32_QByteArray, QModelIndex, QString, QVariant};
use kourier_model::AccountId;
use kourier_viewmodel::AccountRow;
use kourier_viewmodel::diff::RoleSet;

use super::{Store, list_model_glue, role_hash, role_index, row_at};
use crate::bridge::qobject;

const ROLES: [&str; 11] = [
    "accountId",
    "name",
    "network",
    "networkName",
    "networkIcon",
    "status",
    "statusLabel",
    "unread",
    "avatarUrl",
    "number",
    "device",
];

/// Rust side of `AccountListModel`.
#[derive(Default)]
pub struct AccountsRust {
    pub(crate) count: i32,
    pub(crate) store: Store<AccountRow, AccountQt>,
}

/// One account row as Qt values.
pub struct AccountQt {
    id: i64,
    name: QString,
    network: QString,
    network_name: QString,
    network_icon: QString,
    status: QString,
    status_label: QString,
    unread: i32,
    avatar_url: QString,
    number: QString,
    device: QString,
}

impl AccountQt {
    fn from_view(v: &AccountRow) -> Self {
        Self {
            id: v.id.0,
            name: QString::from(v.name.as_str()),
            network: QString::from(v.network.as_str()),
            network_name: QString::from(v.network_name.as_str()),
            network_icon: QString::from(v.network_icon.as_str()),
            status: QString::from(v.status.as_str()),
            status_label: QString::from(v.status_label.as_str()),
            unread: i32::try_from(v.unread).unwrap_or(i32::MAX),
            avatar_url: QString::from(v.avatar_url.as_str()),
            number: QString::from(v.number.as_str()),
            device: QString::from(v.device.as_str()),
        }
    }

    fn changed_roles(a: &AccountRow, b: &AccountRow) -> RoleSet {
        RoleSet::EMPTY
            .with_if(1, a.name != b.name)
            .with_if(2, a.network != b.network)
            .with_if(3, a.network_name != b.network_name)
            .with_if(4, a.network_icon != b.network_icon)
            .with_if(5, a.status != b.status)
            .with_if(6, a.status_label != b.status_label)
            .with_if(7, a.unread != b.unread)
            .with_if(8, a.avatar_url != b.avatar_url)
            .with_if(9, a.number != b.number)
            .with_if(10, a.device != b.device)
    }

    fn key(v: &AccountRow) -> AccountId {
        v.id
    }
}

impl qobject::AccountListModel {
    /// `QAbstractItemModel::data`.
    pub fn data(&self, index: &QModelIndex, role: i32) -> QVariant {
        let (Some(row), Some(role)) = (row_at(&self.store, index), role_index(role, ROLES.len()))
        else {
            return QVariant::default();
        };
        match role {
            0 => QVariant::from(&row.id),
            1 => QVariant::from(&row.name),
            2 => QVariant::from(&row.network),
            3 => QVariant::from(&row.network_name),
            4 => QVariant::from(&row.network_icon),
            5 => QVariant::from(&row.status),
            6 => QVariant::from(&row.status_label),
            7 => QVariant::from(&row.unread),
            8 => QVariant::from(&row.avatar_url),
            9 => QVariant::from(&row.number),
            _ => QVariant::from(&row.device),
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
}

list_model_glue!(qobject::AccountListModel, AccountRow, AccountQt, AccountId);
