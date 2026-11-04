//! `MediaListModel`: the rows of a chat's gallery (people pages P4). `App` owns five
//! (`groupMedia`, `personMedia`: the sections of the two pages, `mediaGrid` and `mediaFiles`:
//! the grid and the list of the level of all media, and the picture viewer's own, which QML does
//! not see as a model: `galleryViewerItems("viewer", message)` reads it), one per [`MediaSlot`]. Rows are
//! `MediaRow`s keyed by the attachment and updated through the diff engine (`diff_or_reset`), so
//! a picture that finishes downloading is one `dataChanged` of `state` and the URLs, a page that
//! is appended is an insert at the end, and a deleted picture is a remove. Besides the rows the
//! model says `more` (more follow the last row: a section's "Show All", the next page of a
//! level), `other` (the other kind has something) and `loaded` (the first answer came).

use cxx_qt_lib::{QHash, QHashPair_i32_QByteArray, QModelIndex, QString, QVariant};
use kourier_model::AttachmentId;
use kourier_viewmodel::chat_media::{MediaRow, MediaSlot, MediaUpdate};
use kourier_viewmodel::diff::RoleSet;
use kourier_viewmodel::media::kind_name;
use std::pin::Pin;

use super::{ListModel, Store, list_model_glue, role_hash, role_index, row_at, set_rows};
use crate::bridge::qobject;

const ROLES: [&str; 17] = [
    "attachment",
    "message",
    "kind",
    "url",
    "fullUrl",
    "placeholderUrl",
    "animated",
    "media",
    "pixelWidth",
    "pixelHeight",
    "mediaState",
    "progress",
    "sizeLabel",
    "fileName",
    "detail",
    "iconName",
    "canSave",
];

/// The role names avoid the names of `Item` and `Control` properties (`width`, `height`, `state`,
/// `icon`): a delegate whose root is an item cannot declare a required property of those names.
/// Rust side of `MediaListModel`.
#[derive(Default)]
pub struct MediaRust {
    pub(crate) count: i32,
    pub(crate) more: bool,
    pub(crate) other: bool,
    pub(crate) loaded: bool,
    pub(crate) store: Store<MediaRow, MediaQt>,
}

/// One media row as Qt values.
pub struct MediaQt {
    attachment: i64,
    message: i64,
    kind: QString,
    url: QString,
    full_url: QString,
    placeholder_url: QString,
    animated: bool,
    media: bool,
    width: i32,
    height: i32,
    state: QString,
    progress: f64,
    label: QString,
    name: QString,
    detail: QString,
    icon: QString,
    save: bool,
}

impl MediaQt {
    fn from_view(v: &MediaRow) -> Self {
        Self {
            attachment: v.attachment.0,
            message: v.message.0,
            kind: QString::from(kind_name(v.kind)),
            url: QString::from(v.url.as_str()),
            full_url: QString::from(v.full_url.as_str()),
            placeholder_url: QString::from(v.placeholder_url.as_str()),
            animated: v.animated,
            media: v.media,
            width: i32::try_from(v.width).unwrap_or(i32::MAX),
            height: i32::try_from(v.height).unwrap_or(i32::MAX),
            state: QString::from(v.state.as_str()),
            progress: f64::from(v.progress),
            label: QString::from(v.label.as_str()),
            name: QString::from(v.name.as_str()),
            detail: QString::from(v.detail.as_str()),
            icon: QString::from(v.icon.as_str()),
            save: v.save,
        }
    }

    fn changed_roles(a: &MediaRow, b: &MediaRow) -> RoleSet {
        MediaRow::changed_roles(a, b)
    }

    fn key(v: &MediaRow) -> AttachmentId {
        v.key()
    }
}

impl qobject::MediaListModel {
    /// `QAbstractItemModel::data`.
    pub fn data(&self, index: &QModelIndex, role: i32) -> QVariant {
        let (Some(row), Some(role)) = (row_at(&self.store, index), role_index(role, ROLES.len()))
        else {
            return QVariant::default();
        };
        match role {
            0 => QVariant::from(&row.attachment),
            1 => QVariant::from(&row.message),
            2 => QVariant::from(&row.kind),
            3 => QVariant::from(&row.url),
            4 => QVariant::from(&row.full_url),
            5 => QVariant::from(&row.placeholder_url),
            6 => QVariant::from(&row.animated),
            7 => QVariant::from(&row.media),
            8 => QVariant::from(&row.width),
            9 => QVariant::from(&row.height),
            10 => QVariant::from(&row.state),
            11 => QVariant::from(&row.progress),
            12 => QVariant::from(&row.label),
            13 => QVariant::from(&row.name),
            14 => QVariant::from(&row.detail),
            15 => QVariant::from(&row.icon),
            _ => QVariant::from(&row.save),
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

    /// Takes a slot's update: the rows through the diff, and the facts.
    pub fn set_media(mut self: Pin<&mut Self>, update: MediaUpdate) {
        let generation = self.store().generation + 1;
        set_rows(self.as_mut(), generation, update.rows);
        if *self.more() != update.more {
            self.as_mut().set_more(update.more);
        }
        if *self.other() != update.other {
            self.as_mut().set_other(update.other);
        }
        if !*self.loaded() {
            self.as_mut().set_loaded(true);
        }
    }

    /// Nothing is shown any more (the slot closed, or opened on another chat): no row, no facts,
    /// not loaded.
    pub fn clear_media(mut self: Pin<&mut Self>) {
        let generation = self.store().generation + 1;
        set_rows(self.as_mut(), generation, Vec::new());
        if *self.more() {
            self.as_mut().set_more(false);
        }
        if *self.other() {
            self.as_mut().set_other(false);
        }
        if *self.loaded() {
            self.as_mut().set_loaded(false);
        }
    }
}

list_model_glue!(qobject::MediaListModel, MediaRow, MediaQt, AttachmentId);

/// A number per gallery slot for the calls about it that wait for the event loop. A call about a
/// slot (`App.showChatMedia`, `closeChatMedia`) that finds the source borrowed (a binding of a page
/// runs while `apply_pending` holds it) is made again from the event loop; by then another call
/// about the same slot may have been made, and the late one must not undo it: every call takes the
/// slot's next number when it is made, and a call that waited runs only if its number is still the
/// slot's latest.
#[derive(Debug, Default)]
pub struct SlotCalls {
    latest: [u64; MediaSlot::COUNT],
}

impl SlotCalls {
    /// A call about `slot` is made: the number it is known by.
    pub fn begin(&mut self, slot: MediaSlot) -> u64 {
        let n = &mut self.latest[slot.index()];
        *n += 1;
        *n
    }

    /// Whether no call about `slot` was made after the one numbered `number`.
    pub fn is_latest(&self, slot: MediaSlot, number: u64) -> bool {
        self.latest[slot.index()] == number
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call that waited for the event loop runs only when nothing was asked of its slot after
    /// it; the other slots are not its business.
    #[test]
    fn a_waiting_call_runs_only_if_it_is_the_latest_of_its_slot() {
        let mut calls = SlotCalls::default();
        let show = calls.begin(MediaSlot::PersonSection);
        assert!(
            calls.is_latest(MediaSlot::PersonSection, show),
            "alone it runs"
        );
        // A close of the same slot comes while the show waits: the show is stale.
        let close = calls.begin(MediaSlot::PersonSection);
        assert!(!calls.is_latest(MediaSlot::PersonSection, show));
        assert!(calls.is_latest(MediaSlot::PersonSection, close));
        // Another slot's calls do not count.
        let other = calls.begin(MediaSlot::Media);
        assert!(calls.is_latest(MediaSlot::PersonSection, close));
        assert!(calls.is_latest(MediaSlot::Media, other));
        // And a later show makes the close stale in its turn (a close never undoes a show).
        let again = calls.begin(MediaSlot::PersonSection);
        assert!(!calls.is_latest(MediaSlot::PersonSection, close));
        assert!(calls.is_latest(MediaSlot::PersonSection, again));
    }
}
