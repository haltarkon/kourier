//! `AttachmentListModel`: the files waiting in the composer's strip, however many. A list model
//! (not a variant list), so a view creates delegates only for the rows on screen and a change
//! touches only the rows it is about.

use cxx_qt_lib::{QHash, QHashPair_i32_QByteArray, QModelIndex, QString, QVariant};
use kourier_viewmodel::FileRow;
use kourier_viewmodel::diff::RoleSet;

use super::{Store, list_model_glue, role_hash, role_index, row_at};
use crate::bridge::qobject;

/// Role names, in role-index order.
pub const ROLES: [&str; 16] = [
    "fileId",
    "name",
    "kind",
    "icon",
    "preview",
    "size",
    "reason",
    "part",
    "partFirst",
    "partLast",
    "partThrough",
    "canOriginal",
    "original",
    "note",
    "stage",
    "progress",
];

/// Rust side of `AttachmentListModel`.
#[derive(Default)]
pub struct AttachmentsRust {
    pub(crate) count: i32,
    pub(crate) store: Store<FileRow, FileQt>,
}

/// One file row as Qt values.
pub struct FileQt {
    id: i64,
    name: QString,
    kind: QString,
    icon: QString,
    preview: QString,
    size: QString,
    reason: QString,
    part: i32,
    part_first: bool,
    part_last: bool,
    part_through: bool,
    can_original: bool,
    original: bool,
    note: QString,
    /// For a folder, where it stands (`counting`, `asking`, `listing`, `packing`); empty
    /// for a file.
    stage: QString,
    /// For a folder that is being packed: how far, 0 to 1.
    progress: f64,
}

impl FileQt {
    fn from_view(v: &FileRow) -> Self {
        Self {
            id: i64::try_from(v.id).unwrap_or(i64::MAX),
            name: QString::from(v.name.as_str()),
            kind: QString::from(v.kind),
            icon: QString::from(v.icon),
            // Only a file whose first bytes are those of an allow-listed picture format is
            // handed to an `Image` (Qt picks the decoder by content; §8.6).
            preview: if v.picture {
                QString::from(crate::app::file_url(&v.path).as_str())
            } else {
                QString::default()
            },
            size: QString::from(v.size.as_str()),
            reason: QString::from(v.reason.as_str()),
            part: i32::try_from(v.part).unwrap_or(i32::MAX),
            part_first: v.part_first,
            part_last: v.part_last,
            part_through: v.part_through,
            can_original: v.can_original,
            original: v.original,
            note: QString::from(v.note.as_str()),
            stage: QString::from(v.stage),
            progress: f64::from(v.progress) / 1000.0,
        }
    }

    fn changed_roles(a: &FileRow, b: &FileRow) -> RoleSet {
        RoleSet::EMPTY
            .with_if(1, a.name != b.name)
            .with_if(2, a.kind != b.kind)
            .with_if(3, a.icon != b.icon)
            .with_if(4, a.picture != b.picture || a.path != b.path)
            .with_if(5, a.size != b.size)
            .with_if(6, a.reason != b.reason)
            .with_if(7, a.part != b.part)
            .with_if(8, a.part_first != b.part_first)
            .with_if(9, a.part_last != b.part_last)
            .with_if(10, a.part_through != b.part_through)
            .with_if(11, a.can_original != b.can_original)
            .with_if(12, a.original != b.original)
            .with_if(13, a.note != b.note)
            .with_if(14, a.stage != b.stage)
            .with_if(15, a.progress != b.progress)
    }

    fn key(v: &FileRow) -> u64 {
        v.id
    }
}

impl qobject::AttachmentListModel {
    /// `QAbstractItemModel::data`.
    pub fn data(&self, index: &QModelIndex, role: i32) -> QVariant {
        let (Some(row), Some(role)) = (row_at(&self.store, index), role_index(role, ROLES.len()))
        else {
            return QVariant::default();
        };
        match role {
            0 => QVariant::from(&row.id),
            1 => QVariant::from(&row.name),
            2 => QVariant::from(&row.kind),
            3 => QVariant::from(&row.icon),
            4 => QVariant::from(&row.preview),
            5 => QVariant::from(&row.size),
            6 => QVariant::from(&row.reason),
            7 => QVariant::from(&row.part),
            8 => QVariant::from(&row.part_first),
            9 => QVariant::from(&row.part_last),
            10 => QVariant::from(&row.part_through),
            11 => QVariant::from(&row.can_original),
            12 => QVariant::from(&row.original),
            13 => QVariant::from(&row.note),
            14 => QVariant::from(&row.stage),
            _ => QVariant::from(&row.progress),
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

    /// The viewmodel rows currently shown (tests).
    pub fn rows(&self) -> &[FileRow] {
        &self.store.view
    }
}

list_model_glue!(qobject::AttachmentListModel, FileRow, FileQt, u64);
