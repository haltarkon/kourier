//! `FilterListModel`: the saved RegExp filters of Settings and the filter dialog's helper
//! (RegExp filters R2), `App.filters`. Rows are the viewmodel's [`FilterRow`]s keyed by the
//! filter's id and updated through the diff engine, so a Move Down is one move and a Notify
//! switch one `dataChanged`. Everything the dialog needs is the viewmodel's: the live check
//! (`checkPattern`), when Save is allowed (`canSave`), the swatches and the default colour; the
//! core's refusal of a save arrives as the `refused` signal under the input it names, and a save
//! the core took as `saved`. Nothing here is logged: names and patterns are the user's.

use cxx_qt::CxxQtType;
use cxx_qt_lib::{QHash, QHashPair_i32_QByteArray, QModelIndex, QString, QStringList, QVariant};
use kourier_app::facade::FilterId;
use kourier_viewmodel::filters::{
    FilterRow, SWATCHES, can_save, check_pattern, default_colour, name_from_pattern, spec_of,
};
use std::pin::Pin;

use super::{ListModel, Store, list_model_glue, role_hash, role_index, row_at, set_rows};
use crate::bridge::qobject;
use crate::shared::Shared;

const ROLES: [&str; 9] = [
    "filterId",
    "name",
    "fields",
    "colour",
    "highlight",
    "notify",
    "notifyMuted",
    "isFirst",
    "isLast",
];

/// Rust side of `FilterListModel`.
#[derive(Default)]
pub struct FiltersRust {
    pub(crate) count: i32,
    /// The core's list is known (the "no filters yet" sentence waits for it).
    pub(crate) loaded: bool,
    /// A save of the dialog waits for the core's answer.
    pub(crate) pending: bool,
    /// The row source (the core's filters are read and changed through it).
    pub(crate) shared: Option<Shared>,
    pub(crate) store: Store<FilterRow, FilterQt>,
}

/// One filter row as Qt values.
pub struct FilterQt {
    id: i64,
    name: QString,
    fields: QString,
    colour: QString,
    highlight: bool,
    notify: bool,
    notify_muted: bool,
    first: bool,
    last: bool,
}

/// `#rrggbb` of a colour value.
pub(crate) fn colour_text(colour: u32) -> String {
    format!("#{:06x}", colour & 0xFF_FFFF)
}

/// The colour value of `#rrggbb` (`None` for anything else).
fn parse_colour(text: &str) -> Option<u32> {
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

impl FilterQt {
    fn from_view(v: &FilterRow) -> Self {
        Self {
            id: v.id,
            name: QString::from(v.name.as_str()),
            fields: QString::from(v.fields.as_str()),
            colour: QString::from(colour_text(v.colour).as_str()),
            highlight: v.highlight,
            notify: v.notify,
            notify_muted: v.notify_muted,
            first: v.first,
            last: v.last,
        }
    }

    fn changed_roles(a: &FilterRow, b: &FilterRow) -> kourier_viewmodel::diff::RoleSet {
        FilterRow::changed_roles(a, b)
    }

    fn key(v: &FilterRow) -> i64 {
        v.key()
    }
}

impl qobject::FilterListModel {
    /// `QAbstractItemModel::data`.
    pub fn data(&self, index: &QModelIndex, role: i32) -> QVariant {
        let (Some(row), Some(role)) = (row_at(&self.store, index), role_index(role, ROLES.len()))
        else {
            return QVariant::default();
        };
        match role {
            0 => QVariant::from(&row.id),
            1 => QVariant::from(&row.name),
            2 => QVariant::from(&row.fields),
            3 => QVariant::from(&row.colour),
            4 => QVariant::from(&row.highlight),
            5 => QVariant::from(&row.notify),
            6 => QVariant::from(&row.notify_muted),
            7 => QVariant::from(&row.first),
            _ => QVariant::from(&row.last),
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

    /// The settings page is shown: the core's filters are asked for.
    pub fn load(self: Pin<&mut Self>) {
        if let Some(shared) = self.shared.clone() {
            shared.borrow_mut().source.load_filters();
        }
    }

    /// Whether the core has said what the filters are (an empty list is then an answer).
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// The live check of one pattern: empty when it is fine (or empty), else the sentence for
    /// the line under its field, with the place ("at character N").
    pub fn check_pattern(&self, pattern: &QString) -> QString {
        QString::from(
            check_pattern(&pattern.to_string())
                .unwrap_or_default()
                .as_str(),
        )
    }

    /// The name "Save as Filter" gives `pattern` (the viewmodel's `name_from_pattern`).
    pub fn name_from_pattern(&self, pattern: &QString) -> QString {
        QString::from(name_from_pattern(&pattern.to_string()).as_str())
    }

    /// Whether Save may be enabled: a name, one pattern at least, every filled pattern valid.
    pub fn can_save(
        &self,
        name: &QString,
        text: &QString,
        sender: &QString,
        chat: &QString,
    ) -> bool {
        can_save(
            &name.to_string(),
            &text.to_string(),
            &sender.to_string(),
            &chat.to_string(),
        )
    }

    /// The eight swatches as `#rrggbb`.
    pub fn swatches(&self) -> QStringList {
        let mut list = QStringList::default();
        for c in SWATCHES {
            list.append(QString::from(colour_text(c).as_str()));
        }
        list
    }

    /// The colour a new filter starts with: the first swatch no filter uses.
    pub fn default_colour(&self) -> QString {
        let used: Vec<u32> = self.store.view.iter().map(|r| r.colour).collect();
        QString::from(colour_text(default_colour(&used)).as_str())
    }

    /// One value of filter `id` for the dialog's prefill: `name`, `text`, `sender`, `chat`
    /// (the patterns), `matchCase`, `notify`, `notifyMuted`, `highlight`, `colour`. Empty for
    /// an unknown id or key.
    pub fn value(&self, id: i64, key: &QString) -> QVariant {
        let Some(row) = self.store.view.iter().find(|r| r.id == id) else {
            return QVariant::default();
        };
        match key.to_string().as_str() {
            "name" => QVariant::from(&QString::from(row.name.as_str())),
            "text" => QVariant::from(&QString::from(row.text.as_str())),
            "sender" => QVariant::from(&QString::from(row.sender.as_str())),
            "chat" => QVariant::from(&QString::from(row.chat.as_str())),
            "matchCase" => QVariant::from(&row.match_case),
            "notify" => QVariant::from(&row.notify),
            "notifyMuted" => QVariant::from(&row.notify_muted),
            "highlight" => QVariant::from(&row.highlight),
            "colour" => QVariant::from(&QString::from(colour_text(row.colour).as_str())),
            _ => QVariant::default(),
        }
    }

    /// Saves the dialog: a new filter (`id` 0) or the edit of `id`. The core validates it
    /// again; `saved` or `refused` follows. `false`: nothing left (no core, a colour that is no
    /// `#rrggbb`).
    #[allow(clippy::too_many_arguments)] // The dialog's inputs, one each.
    pub fn save(
        mut self: Pin<&mut Self>,
        id: i64,
        name: &QString,
        text: &QString,
        sender: &QString,
        chat: &QString,
        match_case: bool,
        notify: bool,
        notify_muted: bool,
        highlight: bool,
        colour: &QString,
    ) -> bool {
        let (Some(shared), Some(colour)) = (self.shared.clone(), parse_colour(&colour.to_string()))
        else {
            return false;
        };
        let spec = spec_of(
            &name.to_string(),
            &text.to_string(),
            &sender.to_string(),
            &chat.to_string(),
            match_case,
            notify,
            notify_muted,
            highlight,
            colour,
        );
        let id = (id != 0).then_some(FilterId(id));
        if !shared.borrow_mut().source.save_filter(id, spec) {
            return false;
        }
        self.as_mut().rust_mut().pending = true;
        true
    }

    /// Switches Notify of filter `id` (the row's switch), keeping everything else.
    pub fn set_notify(self: Pin<&mut Self>, id: i64, on: bool) -> bool {
        let Some(row) = self.store.view.iter().find(|r| r.id == id).cloned() else {
            return false;
        };
        let Some(shared) = self.shared.clone() else {
            return false;
        };
        let spec = spec_of(
            &row.name,
            &row.text,
            &row.sender,
            &row.chat,
            row.match_case,
            on,
            row.notify_muted,
            row.highlight,
            row.colour,
        );
        let mut source = shared.borrow_mut();
        source.source.save_filter(Some(FilterId(id)), spec)
    }

    /// Deletes filter `id` (the page asked first).
    pub fn remove(self: Pin<&mut Self>, id: i64) -> bool {
        let Some(shared) = self.shared.clone() else {
            return false;
        };
        let mut source = shared.borrow_mut();
        source.source.delete_filter(FilterId(id))
    }

    /// Moves filter `id` one place up (`delta` -1) or down (1).
    pub fn move_by(self: Pin<&mut Self>, id: i64, delta: i32) -> bool {
        let Some(at) = self.store.view.iter().position(|r| r.id == id) else {
            return false;
        };
        let to = if delta < 0 {
            at.saturating_sub(1)
        } else {
            (at + 1).min(self.store.view.len().saturating_sub(1))
        };
        if to == at {
            return false;
        }
        let Some(shared) = self.shared.clone() else {
            return false;
        };
        let mut source = shared.borrow_mut();
        source.source.move_filter(FilterId(id), to)
    }

    /// The source's filters now (App calls this after `Changes::filters`): the rows follow, and
    /// a refusal of the dialog's save is told under its input, or a taken save is told as
    /// `saved`.
    pub fn refresh(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let (rows, refusal) = {
            let mut s = shared.borrow_mut();
            (s.source.filter_rows(), s.source.take_filter_refusal())
        };
        if let Some(rows) = rows {
            let generation = self.store().generation + 1;
            set_rows(self.as_mut(), generation, rows);
            if !self.loaded {
                self.as_mut().rust_mut().loaded = true;
                self.as_mut().loaded_changed();
            }
        }
        if let Some(note) = refusal {
            self.as_mut().rust_mut().pending = false;
            self.refused(
                &QString::from(note.field),
                &QString::from(note.text.as_str()),
            );
        } else if self.pending {
            self.as_mut().rust_mut().pending = false;
            self.saved();
        }
    }
}

list_model_glue!(qobject::FilterListModel, FilterRow, FilterQt, i64);
